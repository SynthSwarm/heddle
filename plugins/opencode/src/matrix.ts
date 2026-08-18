/**
 * Matrix transport: crypto, threads, and the progressive edit chain.
 *
 * Two things here were learned the hard way in the phase 0 spike and are not obvious
 * from the SDK's surface:
 *
 *  1. The rust crypto store persists only to IndexedDB, which Node does not have. With
 *     an in-memory store the device ID stays the same while the Olm account is recreated
 *     on every start, so the server holds one set of identity keys and this process
 *     uploads another. Nothing errors; other people's clients simply stop being able to
 *     decrypt. `node-indexeddb` fixes it, but its cache must be loaded before the auto
 *     module is imported or it silently hands back a store that starts empty.
 *
 *  2. Local room state lags /sync, and matrix-js-sdk decides whether to encrypt from
 *     that local state. Sending before the room and its encryption flag have arrived can
 *     put plaintext into a room that is encrypted server-side.
 */

import * as sdk from "matrix-js-sdk";
import type { Logger as SdkLogger } from "matrix-js-sdk/lib/logger.js";
import type { MatrixClient, Room } from "matrix-js-sdk";
import fs from "node:fs/promises";
import path from "node:path";
import { CONTENT_KEY, type AgentEvent } from "./protocol.js";
import type { Config } from "./config.js";

export interface Reaction {
	/** The event being reacted to. */
	targetId: string;
	/** The emoji. */
	key: string;
	/** Who reacted. */
	sender: string;
}

export interface Transport {
	/** Send a new event, returning its ID so later edits can target it. */
	send(roomId: string, threadRoot: string | null, body: string, event: AgentEvent): Promise<string>;
	/** Replace a previously sent event, mirroring the extension into m.new_content. */
	edit(roomId: string, target: string, body: string, event: AgentEvent): Promise<void>;
	/** Open a new thread and return its root event ID. */
	openThread(roomId: string, title: string): Promise<string>;
	/** Watch for reactions from anyone other than us. */
	onReaction(handler: (reaction: Reaction) => void): void;
	close(): Promise<void>;
	readonly userId: string;
	readonly deviceId: string;
}

/**
 * Load the persistent IndexedDB shim, rooted at our own directory.
 *
 * `node-indexeddb` resolves its LevelDB directory as `path.resolve(process.cwd(),
 * "indexeddb")` in its module body, with no override, and the manager is a
 * first-call-wins singleton -- so the only way to place the store is to control the
 * working directory across that one import.
 *
 * That matters more than it looks: opencode runs from whatever project the user is in,
 * so the default would put a different crypto store in every repository, mint a new Olm
 * account each time, and leave an `indexeddb/` directory behind in each. The cwd is
 * restored immediately; the manager keeps the absolute path it resolved.
 */
async function installIndexedDb(storeDir: string): Promise<void> {
	if (typeof (globalThis as { indexedDB?: unknown }).indexedDB !== "undefined") return;
	await fs.mkdir(storeDir, { recursive: true });
	const previous = process.cwd();
	try {
		process.chdir(storeDir);
		const { default: dbManager } = await import("node-indexeddb/dbManager");
		await dbManager.loadCache();
		await import("node-indexeddb/auto");
	} finally {
		process.chdir(previous);
	}
}

/**
 * The IndexedDB database name prefix.
 *
 * Separate from the directory: the directory decides which files on disk hold the store,
 * this decides which database inside them. Both must be stable, and conflating them is
 * how a store that is present on disk still comes back empty.
 */
function databasePrefix(storeDir: string): string {
	return `${path.join(storeDir, "store")}/`;
}

/**
 * A logger that goes nowhere.
 *
 * matrix-js-sdk logs to `console` by default, and inside an opencode plugin that console
 * is the TUI: a single connection attempt printed hundreds of rust-crypto tracing lines
 * straight over the user's terminal. A bridge is a background concern and has no business
 * writing to the screen at all, so its logging is turned off rather than turned down.
 *
 * Set `HEDDLE_MATRIX_DEBUG=1` to get it back on stderr when something needs diagnosing.
 */
function quietLogger(): SdkLogger {
    const debug = process.env.HEDDLE_MATRIX_DEBUG === "1";
    const write =
        debug
            ? (level: string, args: unknown[]) => process.stderr.write(`[matrix:${level}] ${args.join(" ")}\n`)
            : () => {};
    const logger = {
        trace: (...a: unknown[]) => write("trace", a),
        debug: (...a: unknown[]) => write("debug", a),
        info: (...a: unknown[]) => write("info", a),
        warn: (...a: unknown[]) => write("warn", a),
        error: (...a: unknown[]) => write("error", a),
        getChild: () => logger,
    };
    return logger as unknown as SdkLogger;
}

/**
 * The session this bridge owns, kept beside its crypto store.
 *
 * The two belong together: a crypto store is meaningless without the device whose keys
 * it holds. Keeping the token here rather than in the environment also means a fresh
 * store is self-healing — it logs in, gets its own device, and stops fighting over one
 * that already has keys on the server.
 */
interface SavedSession {
	userId: string;
	deviceId: string;
	accessToken: string;
}

async function loadSession(dir: string): Promise<SavedSession | null> {
	try {
		const raw = await fs.readFile(path.join(dir, "session.json"), "utf8");
		const parsed = JSON.parse(raw) as SavedSession;
		return parsed.accessToken && parsed.deviceId && parsed.userId ? parsed : null;
	} catch {
		return null;
	}
}

async function saveSession(dir: string, session: SavedSession): Promise<void> {
	const file = path.join(dir, "session.json");
	await fs.writeFile(file, `${JSON.stringify(session, null, 1)}\n`, { mode: 0o600 });
	// Written with 0600 above, but an existing file keeps its old mode.
	await fs.chmod(file, 0o600).catch(() => {});
}

/**
 * Log in to get a device of our own.
 *
 * Used when the crypto store is new and the configured token belongs to a device the
 * server already holds keys for. Uploading a second set of keys under that device ID is
 * exactly the silent-undecryptable failure the identity check exists to prevent, so the
 * bridge takes its own device instead of colliding with one it does not own.
 */
async function login(config: Config): Promise<SavedSession> {
	const res = await fetch(`${config.homeserver}/_matrix/client/v3/login`, {
		method: "POST",
		headers: { "Content-Type": "application/json" },
		body: JSON.stringify({
			type: "m.login.password",
			identifier: { type: "m.id.user", user: config.userName ?? "" },
			password: config.password,
			initial_device_display_name: "heddle-opencode",
		}),
	});
	if (!res.ok) {
		throw new Error(
			`login failed (HTTP ${res.status}); the bridge needs its own device and could not create one`,
		);
	}
	const body = (await res.json()) as {
		user_id: string;
		device_id: string;
		access_token: string;
	};
	return {
		userId: body.user_id,
		deviceId: body.device_id,
		accessToken: body.access_token,
	};
}

/**
 * Does the server already hold device keys for this device?
 *
 * A fresh crypto store will mint a new Olm account, so adopting a device the server has
 * keys for means uploading a second identity under the same device ID — after which
 * nobody can decrypt what we send. Detecting it up front lets the bridge take its own
 * device instead of failing.
 */
async function deviceKeysDiffer(config: Config, session: SavedSession): Promise<boolean> {
	const res = await fetch(`${config.homeserver}/_matrix/client/v3/keys/query`, {
		method: "POST",
		headers: {
			Authorization: `Bearer ${session.accessToken}`,
			"Content-Type": "application/json",
		},
		body: JSON.stringify({ device_keys: { [session.userId]: [session.deviceId] } }),
	});
	if (!res.ok) return false;
	const body = (await res.json()) as {
		device_keys?: Record<string, Record<string, { keys?: Record<string, string> }>>;
	};
	return Boolean(body.device_keys?.[session.userId]?.[session.deviceId]?.keys);
}

/**
 * Run `body` with console output suppressed.
 *
 * `node-indexeddb` writes progress straight to `console.log` — "oldVersion 11 newVersion
 * 12", one line per database open — with no flag to turn it off. Inside an opencode
 * plugin that console is the user's TUI. Patching a global is unpleasant, so it is done
 * for the narrowest window that works, always restored, and skipped entirely when
 * HEDDLE_MATRIX_DEBUG is set.
 */
async function withoutConsoleNoise<T>(body: () => Promise<T>): Promise<T> {
	if (process.env.HEDDLE_MATRIX_DEBUG === "1") return body();
	const saved = {
		log: console.log,
		info: console.info,
		debug: console.debug,
		warn: console.warn,
	};
	const sink = () => {};
	console.log = sink;
	console.info = sink;
	console.debug = sink;
	console.warn = sink;
	try {
		return await body();
	} finally {
		console.log = saved.log;
		console.info = saved.info;
		console.debug = saved.debug;
		console.warn = saved.warn;
	}
}

export async function connect(config: Config): Promise<Transport> {
	return withoutConsoleNoise(() => connectInner(config));
}

async function connectInner(config: Config): Promise<Transport> {
    // Before anything else: the global logger is what the crypto layer and the WASM
    // tracing bridge reach for, and they are the loudest part of the SDK by far. It is
    // not re-exported from the package root, hence the deep import.
    const quiet = quietLogger();
    const { logger: globalLogger } = await import("matrix-js-sdk/lib/logger.js");
    const quietRecord = quiet as unknown as Record<string, unknown>;
    for (const key of ["trace", "debug", "info", "warn", "error"] as const) {
        (globalLogger as unknown as Record<string, unknown>)[key] = quietRecord[key];
    }

    await installIndexedDb(config.storePath);

	// Prefer a session this bridge owns. Falling back to the configured token is what
	// the first version did throughout, and it works right up until the crypto store is
	// new while the server already holds keys for that token's device — which is a fresh
	// checkout, a moved store, or a second machine.
	let session = await loadSession(config.storePath);

	if (!session) {
		const whoami = await fetch(`${config.homeserver}/_matrix/client/v3/account/whoami`, {
			headers: { Authorization: `Bearer ${config.accessToken}` },
		});
		if (!whoami.ok) {
			throw new Error(`Matrix whoami failed (HTTP ${whoami.status}): check MATRIX_ACCESS_TOKEN`);
		}
		const me = (await whoami.json()) as { user_id: string; device_id?: string };

		// A device-less token comes from Synapse's admin login API. E2EE is per-device,
		// so this is the wrong kind of credential rather than a setting to enable later,
		// and failing here beats failing later as "unable to decrypt" elsewhere.
		if (!me.device_id) {
			if (!config.password) {
				throw new Error(
					"Matrix access token is not device-scoped (no device_id), and no MATRIX_PASSWORD " +
						"is set to log in with. E2EE needs a device.",
				);
			}
			session = await login(config);
			await saveSession(config.storePath, session);
		} else {
			const candidate = {
				userId: me.user_id,
				deviceId: me.device_id,
				accessToken: config.accessToken,
			};
			// Would adopting this device mean overwriting keys the server already has?
			// If so, take a device of our own rather than corrupting one in use.
			const conflict = await deviceKeysDiffer(config, candidate);
			if (conflict && config.password) {
				session = await login(config);
				await saveSession(config.storePath, session);
			} else {
				session = candidate;
				await saveSession(config.storePath, session);
			}
		}
	}

	const client = sdk.createClient({
		baseUrl: config.homeserver,
		accessToken: session.accessToken,
		userId: session.userId,
		deviceId: session.deviceId,
		logger: quiet,
	});

	await client.initRustCrypto({
		useIndexedDB: true,
		cryptoDatabasePrefix: databasePrefix(config.storePath),
	});

	// The store is now load-bearing for a security property, so check rather than trust:
	// if the keys this process holds are not the keys the server has for this device,
	// every message sent will be undecryptable to everyone else. Better to refuse.
	await assertStableIdentity(client, config, session.userId, session.deviceId);

	await client.startClient({ initialSyncLimit: 20 });
	await waitForSync(client);

	return new MatrixTransport(client, session.userId, session.deviceId);
}

/** Compare the device keys we hold against the ones the server has for this device. */
async function assertStableIdentity(
	client: MatrixClient,
	config: Config,
	userId: string,
	deviceId: string,
): Promise<void> {
	const crypto = client.getCrypto();
	if (!crypto) throw new Error("crypto failed to initialise");
	const local = await crypto.getOwnDeviceKeys();

	const res = await fetch(`${config.homeserver}/_matrix/client/v3/keys/query`, {
		method: "POST",
		headers: {
			Authorization: `Bearer ${config.accessToken}`,
			"Content-Type": "application/json",
		},
		body: JSON.stringify({ device_keys: { [userId]: [deviceId] } }),
	});
	if (!res.ok) return; // Not worth failing startup over a check that could not run.

	const body = (await res.json()) as {
		device_keys?: Record<string, Record<string, { keys?: Record<string, string> }>>;
	};
	const remote = body.device_keys?.[userId]?.[deviceId]?.keys?.[`ed25519:${deviceId}`];
	if (!remote) return; // First run for this device: nothing uploaded yet, nothing to contradict.

	if (remote !== local.ed25519) {
		throw new Error(
			`crypto store is not persisting: the server has ed25519 ${remote} for device ` +
				`${deviceId} but this process generated ${local.ed25519}. Messages sent now would ` +
				`be undecryptable. Check that HEDDLE_MATRIX_STORE is writable.`,
		);
	}
}

function waitForSync(client: MatrixClient, timeoutMs = 60_000): Promise<void> {
	return new Promise((resolve, reject) => {
		const timer = setTimeout(() => reject(new Error("initial sync timed out")), timeoutMs);
		client.once(sdk.ClientEvent.Sync, (state: string) => {
			if (state === "PREPARED") {
				clearTimeout(timer);
				resolve();
			}
		});
	});
}

class MatrixTransport implements Transport {
	constructor(
		private readonly client: MatrixClient,
		readonly userId: string,
		readonly deviceId: string,
	) {}

	/**
	 * Wait until the room is in local state and, if it is encrypted server-side, until
	 * the client knows that. Sending before this is how plaintext lands in an encrypted
	 * room.
	 */
	private async room(roomId: string, timeoutMs = 30_000): Promise<Room> {
		const deadline = Date.now() + timeoutMs;
		const crypto = this.client.getCrypto();
		for (;;) {
			const room = this.client.getRoom(roomId);
			if (room) {
				const encrypted = crypto ? await crypto.isEncryptionEnabledInRoom(roomId) : false;
				const stateHasEncryption = Boolean(
					room.currentState.getStateEvents("m.room.encryption", ""),
				);
				// Agree with the room state before sending: if the state says encrypted and
				// the crypto layer does not yet, waiting is the only safe move.
				if (encrypted || !stateHasEncryption) return room;
			}
			if (Date.now() > deadline) {
				if (room) return room;
				throw new Error(`room ${roomId} never arrived in sync`);
			}
			await new Promise((r) => setTimeout(r, 250));
		}
	}

	private relatesToThread(threadRoot: string | null, latest: string | null) {
		if (!threadRoot) return {};
		return {
			"m.relates_to": {
				rel_type: "m.thread",
				event_id: threadRoot,
				is_falling_back: true,
				"m.in_reply_to": { event_id: latest ?? threadRoot },
			},
		};
	}

	async openThread(roomId: string, title: string): Promise<string> {
		await this.room(roomId);
		// Sending logs through a child logger the SDK created at import time, which the
		// quiet logger installed later does not reach, so it lands on stdout -- the TUI.
		return withoutConsoleNoise(async () => {
			const res = await this.client.sendEvent(roomId, "m.room.message" as never, {
				msgtype: "m.text",
				body: title,
			} as never);
			return res.event_id;
		});
	}

	async send(
		roomId: string,
		threadRoot: string | null,
		body: string,
		event: AgentEvent,
	): Promise<string> {
		await this.room(roomId);
		const content = {
			msgtype: "m.notice",
			body,
			...this.relatesToThread(threadRoot, null),
			[CONTENT_KEY]: event,
		};
		return withoutConsoleNoise(async () => {
			const res = await this.client.sendEvent(roomId, "m.room.message" as never, content as never);
			return res.event_id;
		});
	}

	/**
	 * The extension goes in both the top level and `m.new_content`.
	 *
	 * SPEC §3.2 requires the mirror so that a client reading only the resolved edit still
	 * sees the structure. Omitting it means heddle reads the pre-edit state forever and
	 * every completed tool call stays stuck on "running".
	 */
	async edit(roomId: string, target: string, body: string, event: AgentEvent): Promise<void> {
		await this.room(roomId);
		const content = {
			msgtype: "m.notice",
			body: `* ${body}`,
			"m.new_content": {
				msgtype: "m.notice",
				body,
				[CONTENT_KEY]: event,
			},
			"m.relates_to": { rel_type: "m.replace", event_id: target },
			[CONTENT_KEY]: event,
		};
		await withoutConsoleNoise(async () => {
			await this.client.sendEvent(roomId, "m.room.message" as never, content as never);
		});
	}

	async close(): Promise<void> {
		this.client.stopClient();
	}

	/**
	 * Watch for reactions.
	 *
	 * Our own are filtered out, or resolving an approval from here would immediately look
	 * like somebody answering it. Encrypted rooms need the decryption to have happened
	 * first, hence the `Event.Decrypted` path as well as the live timeline.
	 */
	onReaction(handler: (reaction: Reaction) => void): void {
		const emit = (event: sdk.MatrixEvent) => {
			if (event.getType() !== "m.reaction") return;
			if (event.getSender() === this.userId) return;
			const relates = event.getContent()["m.relates_to"];
			if (!relates?.event_id || !relates?.key) return;
			handler({
				targetId: relates.event_id,
				key: relates.key,
				sender: event.getSender() ?? "",
			});
		};

		this.client.on(sdk.RoomEvent.Timeline, (event: sdk.MatrixEvent, _room, toStart) => {
			// Pagination replays history; only live events are answers to a live prompt.
			if (toStart) return;
			emit(event);
		});
		this.client.on(sdk.MatrixEventEvent.Decrypted, (event: sdk.MatrixEvent) => emit(event));
	}
}
