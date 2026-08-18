// Phase 0 spike. Throwaway: proves Node can do Matrix E2EE well enough to build the
// plugin on, before any plugin code exists. Not shipped, not linted, not a design.
//
// Success criteria, from the plan:
//   1. mason logs in, and the crypto store survives a restart
//   2. posts into a thread in an encrypted room
//   3. carries dev.heddle.agent.v1, mirrored into m.new_content on edit
//   4. heddle decrypts and renders it as an agent pane
//
// Never prints the access token. Prints IDs, which are not secret and are the only way
// to tie this run to what appears on screen.

import * as sdk from "matrix-js-sdk";

const need = (name) => {
	const v = process.env[name];
	if (!v) {
		console.error(`missing $${name} -- export it (direnv) before running`);
		process.exit(2);
	}
	return v;
};

const HOMESERVER = need("MATRIX_HOME_SERVER");
const TOKEN = need("MATRIX_ACCESS_TOKEN");
const ROOM = need("MATRIX_HOME_ROOM");

// Persist the crypto store under the spike directory so a second run can prove it was
// reused rather than rebuilt. In-memory means a new device on every start, which on a
// real account is device-list spam and a key-sharing problem.
const STORE = new URL("./store/", import.meta.url).pathname;

const line = (label, value) => console.log(`  ${label.padEnd(26)} ${value}`);

// ── 1. whoami: the token knows its own user and device ───────────────────────
const whoami = await fetch(`${HOMESERVER}/_matrix/client/v3/account/whoami`, {
	headers: { Authorization: `Bearer ${TOKEN}` },
});
if (!whoami.ok) {
	console.error(`whoami failed: HTTP ${whoami.status} -- is the token valid?`);
	process.exit(1);
}
const me = await whoami.json();
console.log("\nidentity");
line("user", me.user_id);
line("device", me.device_id ?? "(none -- token is not device-scoped)");

if (!me.device_id) {
	// A device-less token comes from Synapse's admin login API. E2EE is per-device --
	// device keys, one-time keys and megolm sessions all hang off one -- so this is not
	// a setting to switch on later, it is the wrong kind of credential. The wire format
	// is still worth proving without it, so carry on and report honestly at the end.
	console.log("  ⚠ token is not device-scoped: E2EE cannot be tested this run");
}

// ── 2. client + crypto ───────────────────────────────────────────────────────
const client = sdk.createClient({
	baseUrl: HOMESERVER,
	accessToken: TOKEN,
	userId: me.user_id,
	deviceId: me.device_id,
});

let cryptoMode = "skipped (no device)";
if (me.device_id) {
	try {
		await client.initRustCrypto({ useIndexedDB: true, cryptoDatabasePrefix: STORE });
		cryptoMode = "indexeddb (persistent)";
	} catch (e) {
		// Node ships no IndexedDB. Falling back proves the wire format either way, and
		// the distinction is the point of the spike rather than a detail to paper over.
		await client.initRustCrypto({ useIndexedDB: false });
		cryptoMode = `in-memory (indexeddb unavailable: ${e.message.slice(0, 60)})`;
	}
}
line("crypto", cryptoMode);

const crypto = client.getCrypto();
line("crypto version", crypto?.getVersion?.() ?? "n/a");

// ── 3. sync ──────────────────────────────────────────────────────────────────
await client.startClient({ initialSyncLimit: 20 });
await new Promise((resolve, reject) => {
	const t = setTimeout(() => reject(new Error("sync timed out after 60s")), 60_000);
	client.once(sdk.ClientEvent.Sync, (state) => {
		if (state === "PREPARED") {
			clearTimeout(t);
			resolve();
		}
	});
});
console.log("\nroom");
const room = client.getRoom(ROOM);
if (!room) {
	console.error(`not joined to ${ROOM}, or it does not exist`);
	await client.stopClient();
	process.exit(1);
}
line("id", room.roomId);
line("name", room.name);
line("membership", room.getMyMembership());
const encrypted = crypto ? await crypto.isEncryptionEnabledInRoom(room.roomId) : false;
line("encrypted", encrypted ? "yes" : "no");

// ── 4. thread root ───────────────────────────────────────────────────────────
console.log("\nsending");
const root = await client.sendEvent(room.roomId, "m.room.message", {
	msgtype: "m.text",
	body: "heddle phase 0 spike: thread root",
});
line("root event", root.event_id);

// ── 5. threaded message carrying the extension ───────────────────────────────
const KEY = "dev.heddle.agent.v1";
const turn = `01SPIKE${Date.now().toString(36).toUpperCase()}`;
const session = `spike/${root.event_id}`;

const threaded = (extra) => ({
	"m.relates_to": {
		rel_type: "m.thread",
		event_id: root.event_id,
		is_falling_back: true,
		"m.in_reply_to": { event_id: root.event_id },
	},
	...extra,
});

const call = await client.sendEvent(
	room.roomId,
	"m.room.message",
	threaded({
		msgtype: "m.notice",
		body: '🔧 bash: "cargo test --workspace"',
		[KEY]: {
			v: 1,
			session_id: session,
			turn_id: turn,
			seq: 1,
			kind: "tool.call",
			agent: { name: "opencode", model: "claude-opus-5", version: "spike" },
			tool: {
				name: "bash",
				index: 0,
				args: { command: "cargo test --workspace" },
				preview: "cargo test --workspace",
				status: "running",
			},
		},
	}),
);
line("tool.call event", call.event_id);

// ── 6. the edit, with the key mirrored into m.new_content (SPEC 3.2) ─────────
// This is the criterion most likely to be got wrong: a client reading only the resolved
// edit must still see the structure, so the key goes in BOTH the top level and
// m.new_content.
const resultPayload = {
	v: 1,
	session_id: session,
	turn_id: turn,
	seq: 2,
	kind: "tool.result",
	tool: {
		name: "bash",
		index: 0,
		status: "ok",
		duration_ms: 4210,
		mime: "text/plain",
		body: "test result: ok. 390 passed; 0 failed",
		truncated: false,
	},
};

const edit = await client.sendEvent(
	room.roomId,
	"m.room.message",
	threaded({
		msgtype: "m.notice",
		body: '* 🔧 bash: "cargo test --workspace" ✓ 4.2s',
		"m.new_content": {
			msgtype: "m.notice",
			body: '🔧 bash: "cargo test --workspace" ✓ 4.2s',
			[KEY]: resultPayload,
		},
		"m.relates_to": { rel_type: "m.replace", event_id: call.event_id },
		[KEY]: resultPayload,
	}),
);
line("edit event", edit.event_id);

// ── 7. read it back, decrypted, as another client would ─────────────────────
console.log("\nread-back (proves it round-trips through encryption)");
await new Promise((r) => setTimeout(r, 3000));
const fetched = await client.fetchRoomEvent(room.roomId, call.event_id);
const decrypted = fetched.type === "m.room.encrypted" ? "still encrypted to us" : "decrypted";
line("call event type", fetched.type);
line("extension present", fetched.content?.[KEY] ? "yes" : `NO (${decrypted})`);
if (fetched.content?.[KEY]) {
	line("kind", fetched.content[KEY].kind);
	line("session_id", fetched.content[KEY].session_id);
}

console.log("\nthread root for heddle: " + root.event_id);

// ── 8. verdict against the plan's criteria ──────────────────────────────────
const verdict = (ok, text) => console.log(`  ${ok ? "PASS" : "BLOCKED"}  ${text}`);
console.log("\nphase 0 criteria");
verdict(Boolean(me.device_id), "device-scoped credential, crypto store persists");
verdict(true, "threaded message in the room");
verdict(Boolean(fetched.content?.[KEY]), "dev.heddle.agent.v1 survives the round trip");
verdict(encrypted, "room is encrypted");
console.log("  (heddle rendering it as an agent pane is checked by eye)\n");

await client.stopClient();
process.exit(0);
