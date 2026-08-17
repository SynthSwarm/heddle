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
import type { MatrixClient, Room } from "matrix-js-sdk";
import fs from "node:fs/promises";
import path from "node:path";
import { CONTENT_KEY, type AgentEvent } from "./protocol.js";
import type { Config } from "./config.js";

export interface Transport {
	/** Send a new event, returning its ID so later edits can target it. */
	send(roomId: string, threadRoot: string | null, body: string, event: AgentEvent): Promise<string>;
	/** Replace a previously sent event, mirroring the extension into m.new_content. */
	edit(roomId: string, target: string, body: string, event: AgentEvent): Promise<void>;
	/** Open a new thread and return its root event ID. */
	openThread(roomId: string, title: string): Promise<string>;
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

export async function connect(config: Config): Promise<Transport> {
	await installIndexedDb(config.storePath);

	const whoami = await fetch(`${config.homeserver}/_matrix/client/v3/account/whoami`, {
		headers: { Authorization: `Bearer ${config.accessToken}` },
	});
	if (!whoami.ok) {
		throw new Error(`Matrix whoami failed (HTTP ${whoami.status}): check MATRIX_ACCESS_TOKEN`);
	}
	const me = (await whoami.json()) as { user_id: string; device_id?: string };

	// A device-less token comes from Synapse's admin login API. E2EE is per-device, so
	// this is the wrong kind of credential rather than a setting to enable later, and
	// failing here beats failing later as "unable to decrypt" on somebody else's screen.
	if (!me.device_id) {
		throw new Error(
			"Matrix access token is not device-scoped (no device_id). E2EE needs a token from " +
				"a normal login, not one minted by the admin API.",
		);
	}

	const client = sdk.createClient({
		baseUrl: config.homeserver,
		accessToken: config.accessToken,
		userId: me.user_id,
		deviceId: me.device_id,
	});

	await client.initRustCrypto({
		useIndexedDB: true,
		cryptoDatabasePrefix: databasePrefix(config.storePath),
	});

	// The store is now load-bearing for a security property, so check rather than trust:
	// if the keys this process holds are not the keys the server has for this device,
	// every message sent will be undecryptable to everyone else. Better to refuse.
	await assertStableIdentity(client, config, me.user_id, me.device_id);

	await client.startClient({ initialSyncLimit: 20 });
	await waitForSync(client);

	return new MatrixTransport(client, me.user_id, me.device_id);
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
		const res = await this.client.sendEvent(roomId, "m.room.message" as never, {
			msgtype: "m.text",
			body: title,
		} as never);
		return res.event_id;
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
		const res = await this.client.sendEvent(roomId, "m.room.message" as never, content as never);
		return res.event_id;
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
		await this.client.sendEvent(roomId, "m.room.message" as never, content as never);
	}

	async close(): Promise<void> {
		this.client.stopClient();
	}
}
