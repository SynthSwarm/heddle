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
 *  - **A tool is one event, edited.** The transcript renders one card per Matrix event,
 *    so sending `tool.call` and `tool.result` as two events leaves a card stuck on
 *    "running" beside its own result for ever. The call frame is edited into the result,
 *    which means they share the event's single `seq`.
 *
 *  - **Deltas carry cumulative text.** heddle prefix-checks and replaces, so sending the
 *    whole message so far is correct and appending fragments would duplicate.
 */

import type { Transport } from "./matrix.js";
import type { Config } from "./config.js";
import {
	APPROVAL_REACTIONS,
	SCHEMA_VERSION,
	type AgentEvent,
	type Kind,
	type Tool,
	mimeFor,
	previewOf,
	truncate,
} from "./protocol.js";

/** How opencode is told what the human decided. */
export type PermissionResponse = "once" | "always" | "reject";

/** A permission request as opencode reports it. */
export interface Permission {
	id: string;
	type: string;
	sessionID: string;
	messageID?: string;
	title: string;
	metadata?: Record<string, unknown>;
}

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
	/** The Matrix event this tool owns, edited from `running` to its result. */
	eventId: string | null;
	/** The single `seq` that event owns, reused by every frame of it. */
	seq: number;
	sending: boolean;
	/** Latest state that arrived mid-send, applied once the send completes. */
	pending: Part | null;
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

/** Hooks that exist so fixtures can be generated deterministically. */
export interface BridgeOptions {
	/**
	 * Override the turn identifier.
	 *
	 * The real one is time-and-random, which is correct on the wire and useless in a
	 * committed fixture: every regeneration would differ and the drift check would cry
	 * wolf on every run.
	 */
	newTurnId?: () => string;
	/**
	 * Tell opencode what the human decided.
	 *
	 * Injected rather than reached for, so the approval path can be exercised without an
	 * opencode server: this is the one part of the bridge that talks back.
	 */
	respond?: (permission: Permission, response: PermissionResponse) => Promise<void>;
}

/** An approval waiting on a human, keyed by the Matrix event carrying it. */
interface PendingApproval {
	permission: Permission;
	session: SessionState;
	turn: Turn;
	eventId: string;
	seq: number;
	body: string;
}

export class Bridge {
	private readonly sessions = new Map<string, SessionState>();
	private readonly newTurnId: () => string;
	private readonly respond: (p: Permission, r: PermissionResponse) => Promise<void>;
	/** Approvals awaiting an answer, keyed by the Matrix event that asked. */
	private readonly pending = new Map<string, PendingApproval>();

	constructor(
		private readonly transport: Transport,
		private readonly config: Config,
		private readonly log: (msg: string) => void,
		options: BridgeOptions = {},
	) {
		this.newTurnId = options.newTurnId ?? ulid;
		this.respond = options.respond ?? (async () => {});
		this.transport.onReaction((reaction) => {
			void this.onReaction(reaction.targetId, reaction.key, reaction.sender);
		});
	}

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
				id: this.newTurnId(),
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
			slot = {
				index: t.nextToolIndex++,
				eventId: null,
				seq: 0,
				sending: false,
				pending: null,
				resultSent: false,
			};
			t.tools.set(part.id, slot);
		}
		if (slot.resultSent) return;
		// A fast tool can complete while its call frame is still in flight. Dropping the
		// update would leave the card running for ever, so the latest state is held and
		// applied once the send finishes.
		if (slot.sending) {
			slot.pending = part;
			return;
		}

		const state = part.state ?? {};
		const status = state.status ?? "pending";
		if (status === "pending") return;
		const root = await this.thread(s, this.titleFor(s));
		const done = status === "completed" || status === "error";

		slot.sending = true;
		try {
			if (!slot.eventId) {
				// The call frame: a new event, which owns one seq for its whole lifecycle.
				slot.seq = t.nextSeq++;
				const tool: Tool = {
					name,
					index: slot.index,
					args: state.input,
					preview: state.title ?? previewOf(state.input),
					status: "running",
				};
				const ev = this.envelope(s, t, "tool.call", slot.seq);
				ev.tool = tool;
				slot.eventId = await this.transport.send(
					this.config.roomId,
					root,
					`🔧 ${name}${tool.preview ? `: "${tool.preview}"` : ""}`,
					ev,
				);
			}

			if (done) {
				const ok = status === "completed";
				const raw = ok ? (state.output ?? "") : (state.error ?? "failed");
				const { body, truncated } = truncate(raw);
				const start = state.time?.start;
				const end = state.time?.end;
				const tool: Tool = {
					name,
					index: slot.index,
					args: state.input,
					preview: state.title ?? previewOf(state.input),
					status: ok ? "ok" : "error",
					duration_ms: start && end ? Math.max(0, end - start) : undefined,
					mime: mimeFor(name, body),
					body,
					truncated,
				};
				// An edit of the call, not a second event: a tool is one card with a
				// lifecycle, and the transcript renders one card per Matrix event. Sending
				// a separate result leaves a card stuck on "running" beside it for ever.
				// The seq is the call's, because heddle reads the chain resolved and a seq
				// spent on a frame nobody sees would read as a missing event.
				const ev = this.envelope(s, t, "tool.result", slot.seq);
				ev.tool = tool;
				const glyph = ok ? "✓" : "✗";
				await this.transport.edit(
					this.config.roomId,
					slot.eventId,
					`🔧 ${name}${tool.preview ? `: "${tool.preview}"` : ""} ${glyph}`,
					ev,
				);
				slot.resultSent = true;
			}
		} finally {
			slot.sending = false;
			const queued = slot.pending;
			slot.pending = null;
			if (queued) await this.onToolPart(s, t, queued);
		}
	}

	// ── approvals ───────────────────────────────────────────────────────────────

	/**
	 * A tool wants permission. Ask, in the pane the work is happening in.
	 *
	 * The turn is the one the permission belongs to, so the prompt lands in the thread
	 * the user is already watching rather than opening a pane of its own.
	 */
	async onPermission(permission: Permission): Promise<void> {
		if ([...this.pending.values()].some((p) => p.permission.id === permission.id)) return;

		const s = this.session(permission.sessionID);
		const t = this.turn(s, permission.messageID ?? `permission-${permission.id}`);
		const root = await this.thread(s, this.titleFor(s));

		const command =
			typeof permission.metadata?.command === "string"
				? (permission.metadata.command as string)
				: undefined;
		const cwd =
			typeof permission.metadata?.cwd === "string" ? (permission.metadata.cwd as string) : undefined;

		const seq = t.nextSeq++;
		const ev = this.envelope(s, t, "approval.request", seq);
		ev.approval = {
			id: permission.id,
			kind: permission.type,
			command: command ?? permission.title,
			cwd,
			// opencode permissions do not expire, so no countdown is advertised. Sending
			// an invented deadline would put a timer on screen that means nothing.
			reactions: APPROVAL_REACTIONS,
		};
		const body = `⚠ ${permission.title} — approve?`;
		const eventId = await this.transport.send(this.config.roomId, root, body, ev);
		this.pending.set(eventId, { permission, session: s, turn: t, eventId, seq, body });
		this.log(`approval requested: ${permission.title}`);
	}

	/** opencode resolved it elsewhere -- in the TUI, or by another client. */
	async onPermissionReplied(permissionId: string): Promise<void> {
		const entry = [...this.pending.entries()].find(([, p]) => p.permission.id === permissionId);
		if (!entry) return;
		const [eventId, pending] = entry;
		this.pending.delete(eventId);
		await this.resolve(pending, "approve", undefined, "answered elsewhere");
	}

	/**
	 * Somebody reacted. If it answers a live approval, that is the decision.
	 *
	 * Unknown emoji are ignored rather than treated as a denial: a thumbs-up on an
	 * approval prompt is a comment, and guessing what it meant would run a command.
	 */
	private async onReaction(targetId: string, key: string, sender: string): Promise<void> {
		const pending = this.pending.get(targetId);
		if (!pending) return;

		const choice = APPROVAL_REACTIONS[key] ?? APPROVAL_REACTIONS[key.replace(/\uFE0F/g, "")];
		if (!choice) return;

		const response: PermissionResponse =
			choice === "approve" ? "once" : choice === "always" ? "always" : "reject";

		this.pending.delete(targetId);
		try {
			await this.respond(pending.permission, response);
		} catch (e) {
			this.log(`could not pass the answer to opencode: ${(e as Error).message}`);
			// Put it back: the tool is still blocked, and a prompt that silently stops
			// accepting answers is worse than one answered twice.
			this.pending.set(targetId, pending);
			return;
		}
		await this.resolve(pending, choice === "deny" ? "deny" : "approve", sender, response);
	}

	/** Edit the request into its outcome, so the card stops asking. */
	private async resolve(
		pending: PendingApproval,
		choice: "approve" | "deny",
		by: string | undefined,
		detail: string,
	): Promise<void> {
		const { session: s, turn: t } = pending;
		// Same seq as the request, because this is that event's resolved edit rather than
		// a second event -- the same rule tool results follow.
		const ev = this.envelope(s, t, "approval.resolved", pending.seq);
		ev.approval = {
			id: pending.permission.id,
			kind: pending.permission.type,
			choice,
			by,
		};
		const glyph = choice === "approve" ? "✓" : "✗";
		await this.transport.edit(
			this.config.roomId,
			pending.eventId,
			`${glyph} ${pending.permission.title} — ${detail}`,
			ev,
		);
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
			// heddle renders this from the structure, but the body is what every other
			// Matrix client shows. An empty notice reads as a blank message in Element.
			const parts = [
				info.tokens?.input !== undefined ? `${info.tokens.input} in` : null,
				info.tokens?.output !== undefined ? `${info.tokens.output} out` : null,
				info.cost !== undefined ? `$${info.cost.toFixed(4)}` : null,
			].filter(Boolean);
			await this.transport.send(this.config.roomId, root, parts.join(" / "), ev);
		}

		const stop = this.envelope(s, t, "message.stop", t.nextSeq++);
		stop.final = true;
		await this.transport.send(this.config.roomId, root, "— end of turn —", stop);
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
