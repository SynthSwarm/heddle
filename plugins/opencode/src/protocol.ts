/**
 * The `dev.heddle.agent.v1` envelope.
 *
 * This is heddle's schema, not opencode's, and the authoritative definition is the Rust
 * one in `crates/heddle-agent/src/protocol.rs`. These types exist to keep the emitter
 * honest at compile time; the thing that proves the two agree is the conformance
 * fixtures, because a type declaration can be confidently wrong.
 *
 * See `docs/SPEC.md` §3.2.
 */

/** The content key. Reverse-DNS namespaced, so other Matrix clients ignore it. */
export const CONTENT_KEY = "dev.heddle.agent.v1";

/** Schema version. Consumers reject unknown majors. */
export const SCHEMA_VERSION = 1;

export type Kind =
	| "message.delta"
	| "message.stop"
	| "commentary"
	| "tool.call"
	| "tool.result"
	| "notice"
	| "approval.request"
	| "approval.resolved"
	| "model.picker"
	| "usage";

export type ToolStatus = "running" | "ok" | "error";

export interface AgentInfo {
	name: string;
	model?: string;
	version?: string;
}

export interface Tool {
	name: string;
	/** Pairs a `tool.result` to its `tool.call` within the turn. */
	index: number;
	args?: unknown;
	preview?: string;
	status: ToolStatus;
	duration_ms?: number;
	/** Drives which renderer heddle uses. See SPEC §3.2. */
	mime?: string;
	body?: string;
	truncated?: boolean;
}

export interface Usage {
	input_tokens?: number;
	output_tokens?: number;
	cost_usd?: number;
}

export interface Notice {
	kind: string;
	text?: string;
	extra?: Record<string, string>;
}

/**
 * A request for a human decision, and its outcome.
 *
 * `reactions` maps the emoji a client may send to the choice it means. heddle reads this
 * to decide what to send for `y`/`n`, falling back to ✅/❌ when it is absent, so the map
 * is the contract for anything answering from another Matrix client too.
 */
export interface Approval {
	id: string;
	kind: string;
	command?: string;
	cwd?: string;
	/** Unix seconds. Absent means no timeout, which is opencode's behaviour. */
	expires_at?: number;
	reactions?: Record<string, string>;
	choice?: "approve" | "deny" | "timeout";
	/** Matrix user who resolved it. */
	by?: string;
}

/**
 * Emoji offered for an approval, and what each means.
 *
 * `always` has no key in heddle -- its `ApprovalChoice` is approve, deny or timeout --
 * but advertising it costs nothing and lets somebody answer from Element, where the
 * repetitive-tool case that makes "always" worth having actually bites.
 */
export const APPROVAL_REACTIONS: Record<string, string> = {
	"✅": "approve",
	"❌": "deny",
	"♾️": "always",
};

export interface AgentEvent {
	v: number;
	/** Stable for the lifetime of a pane. One opencode session maps to one thread. */
	session_id: string;
	/** Groups every event of a single response. */
	turn_id: string;
	/** Monotonic within a turn. Gaps are how heddle knows it is missing events. */
	seq: number;
	kind: Kind;
	/** Sent on the first event of a turn. */
	agent?: AgentInfo;
	text?: string;
	final?: boolean;
	tool?: Tool;
	notice?: Notice;
	approval?: Approval;
	usage?: Usage;
}

/**
 * MIME type for a tool result, which is what decides how heddle renders the body.
 *
 * Guessing wrong is not cosmetic: `text/x-diff` gets a gutter and add/delete counts,
 * while `text/plain` gets a folded monospace block. A diff announced as plain text loses
 * the single most useful rendering heddle has.
 */
export function mimeFor(tool: string, body: string): string {
	// Content first, tool name second. The name is a hint about what a tool usually
	// returns; the body is evidence about what it returned this time. `webfetch` keyed on
	// its name alone announced a JSON API response as markdown, which costs the
	// collapsible tree and gets a wall of braces rendered as prose.
	if (tool === "edit" || tool === "patch" || tool === "multiedit") {
		// Only claim a diff if it actually looks like one. opencode's edit tool reports
		// its output in more than one shape depending on the model and the file.
		if (/^(---|\+\+\+|@@)/m.test(body)) return "text/x-diff";
	}
	const trimmed = body.trimStart();
	if (trimmed.startsWith("{") || trimmed.startsWith("[")) {
		try {
			JSON.parse(body);
			return "application/json";
		} catch {
			// Not JSON after all; fall through rather than mislabel it.
		}
	}
	if (tool === "webfetch") return "text/markdown";
	return "text/plain";
}

/**
 * Cap a result body.
 *
 * A 40MB `read` result would be sent to a homeserver, encrypted, and then folded to
 * twenty lines on screen. `truncated` exists precisely so the loss is stated rather than
 * hidden, which is the same principle as the `~` marker.
 */
export function truncate(body: string, limit = 64_000): { body: string; truncated: boolean } {
	if (body.length <= limit) return { body, truncated: false };
	return { body: body.slice(0, limit), truncated: true };
}

/** A short human-readable preview of tool arguments, for the card header. */
export function previewOf(input: unknown): string | undefined {
	if (typeof input !== "object" || input === null) return undefined;
	const o = input as Record<string, unknown>;
	for (const key of ["command", "filePath", "path", "pattern", "url", "query"]) {
		const v = o[key];
		if (typeof v === "string" && v.length > 0) return v;
	}
	const json = JSON.stringify(o);
	if (!json || json === "{}") return undefined;
	return json.length > 120 ? `${json.slice(0, 120)}…` : json;
}
