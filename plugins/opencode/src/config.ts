/**
 * Configuration, entirely from the environment.
 *
 * opencode does not load a project `.env`, so these must be exported before opencode
 * starts -- direnv is the usual way. The plugin deliberately does not read `.env`
 * itself: silently reading a credentials file out of the working directory is a
 * surprising thing for a plugin to do.
 */

export interface Config {
	homeserver: string;
	accessToken: string;
	roomId: string;
	storePath: string;
	agentName: string;
	/** Minimum ms between edits of a streaming message. Homeservers rate-limit. */
	editIntervalMs: number;
	/** Characters buffered before an edit is forced regardless of the interval. */
	bufferChars: number;
	/** Emit `commentary` events for reasoning text. */
	commentary: boolean;
	enabled: boolean;
}

function int(name: string, fallback: number): number {
	const raw = process.env[name];
	if (!raw) return fallback;
	const n = Number.parseInt(raw, 10);
	return Number.isFinite(n) ? n : fallback;
}

function bool(name: string, fallback: boolean): boolean {
	const raw = process.env[name];
	if (raw === undefined) return fallback;
	return raw === "1" || raw.toLowerCase() === "true";
}

/**
 * Read the configuration, or explain precisely what is missing.
 *
 * Returns `null` when the bridge is simply not configured, which is the common case for
 * anyone who installed the plugin and has not set it up: that must be silent rather than
 * an error on every opencode start.
 */
export function load(): Config | null {
	const homeserver = process.env.MATRIX_HOME_SERVER;
	const accessToken = process.env.MATRIX_ACCESS_TOKEN;
	const roomId = process.env.HEDDLE_MATRIX_ROOM ?? process.env.MATRIX_HOME_ROOM;

	if (!homeserver && !accessToken && !roomId) return null;

	const missing = [
		!homeserver && "MATRIX_HOME_SERVER",
		!accessToken && "MATRIX_ACCESS_TOKEN",
		!roomId && "HEDDLE_MATRIX_ROOM (or MATRIX_HOME_ROOM)",
	].filter(Boolean);
	if (missing.length > 0) {
		throw new Error(`heddle-opencode is partially configured; missing ${missing.join(", ")}`);
	}

	return {
		homeserver: homeserver!.replace(/\/+$/, ""),
		accessToken: accessToken!,
		roomId: roomId!,
		storePath: process.env.HEDDLE_MATRIX_STORE ?? `${process.env.HOME}/.local/state/heddle-opencode/`,
		agentName: process.env.HEDDLE_AGENT_NAME ?? "opencode",
		editIntervalMs: int("HEDDLE_EDIT_INTERVAL_MS", 1200),
		bufferChars: int("HEDDLE_BUFFER_CHARS", 60),
		commentary: bool("HEDDLE_COMMENTARY", true),
		enabled: bool("HEDDLE_REMOTE", true),
	};
}
