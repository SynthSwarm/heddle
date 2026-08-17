/**
 * opencode's event stream, translated into `dev.heddle.agent.v1`.
 *
 * The rules that matter, all of them learned from reading heddle's own store rather than
 * guessed (`crates/heddle-agent/src/store.rs`):
 *
 *  - **One `seq` per Matrix event.** heddle reads the *resolved* edit chain, so a `seq`
 *    spent on an intermediate edit is never observed. Burning a number per edit leaves
 *    holes in what heddle actually sees, and `store.rs` treats a jump of more than one as
 *    missing events -- the `!` marker -- on every pane.
 *
 *  - **Only `message.delta` is edited.** It is the single kind exempt from the replay
 *    guard, so re-sending it under the same `seq` is allowed and is how streaming works.
 *
 *  - **`tool.call` and `tool.result` are separate events**, paired by `tool.index`, not
 *    an edit of one another. `store.rs` looks the call up by index and updates the card
 *    in place, keeping the arguments the call carried.
 *
 *  - **Deltas carry cumulative text.** heddle prefix-checks and replaces, so sending the
 *    whole message so far is correct and appending fragments would duplicate.
 */

import type { Transport } from "./matrix.js";
import type { Config } from "./config.js";
import {
	SCHEMA_VERSION,
	type AgentEvent,
	type Kind,
	type Tool,
	mimeFor,
	previewOf,
	truncate,
} from "./protocol.js";

const B32 = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/** A ULID-shaped identifier. Sortable by time, which is all the schema asks of it. */
function ulid(): string {
	let time = Date.now();
	let out = "";
	for (let i = 9; i >= 0; i--) {
		out = B32[time % 32]! + out;
		time = Math.floor(time / 32);
	}
	for (let i = 0; i < 16; i++) out += B32[Math.floor(Math.random() * 32)]!;
	return out;
}

interface ToolSlot {
	index: number;
	callSent: boolean;
	resultSent: boolean;
}

interface Turn {
	id: string;
	/** Next `seq` to hand out. One per Matrix event, never per edit. */
	nextSeq: number;
	agentSent: boolean;
	model?: string;
	/** The streaming text event, its owned `seq`, and the text sent so far. */
	textEventId: string | null;
	textSeq: number;
	text: string;
	flushedText: string;
	timer: ReturnType<typeof setTimeout> | null;
	sending: boolean;
	tools: Map<string, ToolSlot>;
	nextToolIndex: number;
	stopped: boolean;
}

interface SessionState {
	sessionId: string;
	threadRoot: string | null;
	opening: Promise<string> | null;
	turns: Map<string, Turn>;
}

type Part = {
	id: string;
	messageID: string;
	sessionID: string;
	type: string;
	text?: string;
	tool?: string;
	callID?: string;
	state?: {
		status?: string;
		input?: unknown;
		output?: string;
		title?: string;
		error?: string;
		time?: { start?: number; end?: number };
		metadata?: Record<string, unknown>;
	};
};

export class Bridge {
	private readonly sessions = new Map<string, SessionState>();

	constructor(
		private readonly transport: Transport,
		private readonly config: Config,
		private readonly log: (msg: string) => void,
	) {}

	private session(sessionID: string): SessionState {
		let s = this.sessions.get(sessionID);
		if (!s) {
			s = { sessionId: sessionID, threadRoot: null, opening: null, turns: new Map() };
			this.sessions.set(sessionID, s);
		}
		return s;
	}

	/**
	 * One opencode session is one Matrix thread, because in heddle a thread is a pane and
	 * a pane is an agent session. Opening is memoised on a promise so that a burst of
	 * events at the start of a turn cannot race and create several roots.
	 */
	private async thread(s: SessionState, title: string): Promise<string> {
		if (s.threadRoot) return s.threadRoot;
		if (!s.opening) {
			s.opening = this.transport
				.openThread(this.config.roomId, title)
				.then((id) => {
					s.threadRoot = id;
					return id;
				})
				.catch((e) => {
					s.opening = null;
					throw e;
				});
		}
		return s.opening;
	}

	private turn(s: SessionState, messageID: string): Turn {
		let t = s.turns.get(messageID);
		if (!t) {
			t = {
				id: ulid(),
				nextSeq: 1,
				agentSent: false,
				textEventId: null,
				textSeq: 0,
				text: "",
				flushedText: "",
				timer: null,
				sending: false,
				tools: new Map(),
				nextToolIndex: 0,
				stopped: false,
			};
			s.turns.set(messageID, t);
		}
		return t;
	}

	private envelope(s: SessionState, t: Turn, kind: Kind, seq: number): AgentEvent {
		const ev: AgentEvent = {
			v: SCHEMA_VERSION,
			session_id: s.sessionId,
			turn_id: t.id,
			seq,
			kind,
		};
		// Identity goes on the first event of the turn only, as the schema specifies.
		if (!t.agentSent) {
			ev.agent = { name: this.config.agentName, model: t.model, version: "0.1.0" };
			t.agentSent = true;
		}
		return ev;
	}

	// ── text ────────────────────────────────────────────────────────────────────

	private scheduleText(s: SessionState, t: Turn): void {
		if (t.timer) return;
		const overflowing = t.text.length - t.flushedText.length >= this.config.bufferChars;
		const delay = overflowing ? 0 : this.config.editIntervalMs;
		t.timer = setTimeout(() => {
			t.timer = null;
			void this.flushText(s, t);
		}, delay);
	}

	private async flushText(s: SessionState, t: Turn): Promise<void> {
		if (t.sending || t.text === t.flushedText || t.text.length === 0) return;
		t.sending = true;
		const text = t.text;
		try {
			const root = await this.thread(s, this.titleFor(s));
			if (!t.textEventId) {
				// First frame: a new event, which owns a seq for the rest of the turn.
				t.textSeq = t.nextSeq++;
				const ev = this.envelope(s, t, "message.delta", t.textSeq);
				ev.text = text;
				t.textEventId = await this.transport.send(this.config.roomId, root, text, ev);
			} else {
				// Later frames edit that event and reuse its seq. A new number here would
				// be invisible to heddle once the chain resolves, and would read as a gap.
				const ev = this.envelope(s, t, "message.delta", t.textSeq);
				ev.text = text;
				await this.transport.edit(this.config.roomId, t.textEventId, text, ev);
			}
			t.flushedText = text;
		} catch (e) {
			this.log(`text flush failed: ${(e as Error).message}`);
		} finally {
			t.sending = false;
			if (t.text !== t.flushedText) this.scheduleText(s, t);
		}
	}

	// ── tools ───────────────────────────────────────────────────────────────────

	private async onToolPart(s: SessionState, t: Turn, part: Part): Promise<void> {
		const name = part.tool ?? "tool";
		// opencode surfaces its ask/question tool as a gate rather than a tool call;
		// echoing its raw input as a card would duplicate the approval UI.
		if (name === "question" || name === "ask") return;

		let slot = t.tools.get(part.id);
		if (!slot) {
			slot = { index: t.nextToolIndex++, callSent: false, resultSent: false };
			t.tools.set(part.id, slot);
		}

		const state = part.state ?? {};
		const status = state.status ?? "pending";
		const root = await this.thread(s, this.titleFor(s));

		if (!slot.callSent && (status === "running" || status === "completed" || status === "error")) {
			const tool: Tool = {
				name,
				index: slot.index,
				args: state.input,
				preview: state.title ?? previewOf(state.input),
				status: "running",
			};
			const ev = this.envelope(s, t, "tool.call", t.nextSeq++);
			ev.tool = tool;
			await this.transport.send(
				this.config.roomId,
				root,
				`🔧 ${name}${tool.preview ? `: "${tool.preview}"` : ""}`,
				ev,
			);
			slot.callSent = true;
		}

		if (!slot.resultSent && (status === "completed" || status === "error")) {
			const ok = status === "completed";
			const raw = ok ? (state.output ?? "") : (state.error ?? "failed");
			const { body, truncated } = truncate(raw);
			const start = state.time?.start;
			const end = state.time?.end;
			const tool: Tool = {
				name,
				index: slot.index,
				status: ok ? "ok" : "error",
				duration_ms: start && end ? Math.max(0, end - start) : undefined,
				mime: mimeFor(name, body),
				body,
				truncated,
			};
			// A separate event, not an edit: heddle pairs it to the call by index and
			// updates that card in place, keeping the arguments the call carried.
			const ev = this.envelope(s, t, "tool.result", t.nextSeq++);
			ev.tool = tool;
			const glyph = ok ? "✓" : "✗";
			await this.transport.send(this.config.roomId, root, `🔧 ${name} ${glyph}`, ev);
			slot.resultSent = true;
		}
	}

	// ── lifecycle ───────────────────────────────────────────────────────────────

	private titles = new Map<string, string>();

	private titleFor(s: SessionState): string {
		return this.titles.get(s.sessionId) ?? `opencode session ${s.sessionId.slice(0, 8)}`;
	}

	setTitle(sessionID: string, title: string): void {
		if (title) this.titles.set(sessionID, title);
	}

	async onPart(part: Part): Promise<void> {
		const s = this.session(part.sessionID);
		const t = this.turn(s, part.messageID);
		if (t.stopped) return;

		switch (part.type) {
			case "text":
				if (typeof part.text === "string") {
					t.text = part.text; // authoritative snapshot; heddle wants cumulative
					this.scheduleText(s, t);
				}
				return;
			case "reasoning":
				if (this.config.commentary && typeof part.text === "string" && part.text.length > 0) {
					const root = await this.thread(s, this.titleFor(s));
					const ev = this.envelope(s, t, "commentary", t.nextSeq++);
					ev.text = part.text;
					await this.transport.send(this.config.roomId, root, part.text, ev);
				}
				return;
			case "tool":
				await this.onToolPart(s, t, part);
				return;
			default:
				return;
		}
	}

	/** Assistant message finished: flush text, then report usage and stop the turn. */
	async onMessageComplete(
		sessionID: string,
		messageID: string,
		info: { modelID?: string; cost?: number; tokens?: { input?: number; output?: number } },
	): Promise<void> {
		const s = this.session(sessionID);
		const t = this.turn(s, messageID);
		if (t.stopped) return;
		t.model = info.modelID ?? t.model;

		if (t.timer) {
			clearTimeout(t.timer);
			t.timer = null;
		}
		await this.flushText(s, t);

		const root = s.threadRoot;
		if (!root) {
			// Nothing was ever sent for this turn -- an empty or aborted response. Opening
			// a thread purely to announce that it stopped would create an empty pane.
			t.stopped = true;
			return;
		}

		if (info.tokens || info.cost !== undefined) {
			const ev = this.envelope(s, t, "usage", t.nextSeq++);
			ev.usage = {
				input_tokens: info.tokens?.input,
				output_tokens: info.tokens?.output,
				cost_usd: info.cost,
			};
			await this.transport.send(this.config.roomId, root, "", ev);
		}

		const stop = this.envelope(s, t, "message.stop", t.nextSeq++);
		stop.final = true;
		await this.transport.send(this.config.roomId, root, "", stop);
		t.stopped = true;
	}

	/** Flush anything still buffered. Called on session.idle and on dispose. */
	async flushAll(): Promise<void> {
		for (const s of this.sessions.values()) {
			for (const t of s.turns.values()) {
				if (t.timer) {
					clearTimeout(t.timer);
					t.timer = null;
				}
				await this.flushText(s, t);
			}
		}
	}
}
