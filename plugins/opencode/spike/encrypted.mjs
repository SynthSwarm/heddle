// Phase 0, encrypted half. Proves the extension survives megolm, not just the wire.
//
// Creates its own encrypted room rather than switching encryption on in the existing
// one: m.room.encryption cannot be turned off again, so flipping it on someone's real
// room to run a test is a one-way door for a throwaway.

import * as sdk from "matrix-js-sdk";
import fs from "node:fs";

const { default: dbManager } = await import("node-indexeddb/dbManager");
await dbManager.loadCache();
await import("node-indexeddb/auto");

const HOMESERVER = process.env.MATRIX_HOME_SERVER;
const TOKEN = process.env.MATRIX_ACCESS_TOKEN;
const INVITE = process.env.SPIKE_INVITE ?? "@quintin:matrix.synthswarm.com";
const STORE = new URL("./store/", import.meta.url).pathname;
const ROOM_FILE = new URL("./encrypted-room.txt", import.meta.url).pathname;

const line = (l, v) => console.log(`  ${l.padEnd(24)} ${v}`);

const who = await (
	await fetch(`${HOMESERVER}/_matrix/client/v3/account/whoami`, {
		headers: { Authorization: `Bearer ${TOKEN}` },
	})
).json();

const client = sdk.createClient({
	baseUrl: HOMESERVER,
	accessToken: TOKEN,
	userId: who.user_id,
	deviceId: who.device_id,
});
await client.initRustCrypto({ useIndexedDB: true, cryptoDatabasePrefix: STORE });
const crypto = client.getCrypto();

console.log("\nidentity");
line("user", who.user_id);
line("device", who.device_id);
const own = await crypto.getOwnDeviceKeys();
line("ed25519", own.ed25519);

await client.startClient({ initialSyncLimit: 20 });
await new Promise((resolve, reject) => {
	const t = setTimeout(() => reject(new Error("sync timed out")), 60_000);
	client.once(sdk.ClientEvent.Sync, (s) => {
		if (s === "PREPARED") {
			clearTimeout(t);
			resolve();
		}
	});
});

// Local room state lags /sync. `getRoom` returns null for a room the server has already
// created, and `isEncryptionEnabledInRoom` answers from that same local state -- so
// sending straight after createRoom can put plaintext into a room that is encrypted
// server-side, and the client will think it did the right thing. Wait for the room and
// for its encryption flag before anything is sent.
const waitForEncryptedRoom = async (id, timeoutMs = 30_000) => {
	const deadline = Date.now() + timeoutMs;
	while (Date.now() < deadline) {
		const r = client.getRoom(id);
		if (r && (await crypto.isEncryptionEnabledInRoom(id))) return r;
		await new Promise((s) => setTimeout(s, 500));
	}
	return client.getRoom(id);
};

// ── room: reuse across runs so an accepted invite is not thrown away ─────────
console.log("\nroom");
let roomId = fs.existsSync(ROOM_FILE) ? fs.readFileSync(ROOM_FILE, "utf8").trim() : null;
// Only treat a recorded room as gone if the server disagrees, not if sync is merely
// behind -- otherwise every run creates another room and invites the user again.
if (roomId) {
	const joined = await client.getJoinedRooms().catch(() => ({ joined_rooms: [] }));
	if (!joined.joined_rooms.includes(roomId)) roomId = null;
}

if (!roomId) {
	const created = await client.createRoom({
		name: "heddle spike (encrypted)",
		topic: "Phase 0: does dev.heddle.agent.v1 survive megolm?",
		invite: [INVITE],
		initial_state: [
			{
				type: "m.room.encryption",
				state_key: "",
				content: { algorithm: "m.megolm.v1.aes-sha2" },
			},
		],
	});
	roomId = created.room_id;
	fs.writeFileSync(ROOM_FILE, roomId);
	line("created", roomId);
	line("invited", INVITE);
} else {
	line("reused", roomId);
}

const room = await waitForEncryptedRoom(roomId);
if (!room) {
	console.error(`room ${roomId} never arrived in sync`);
	process.exit(1);
}
line("encrypted", (await crypto.isEncryptionEnabledInRoom(roomId)) ? "yes" : "NO");
const members = room.getMembersWithMembership("join").map((m) => m.userId);
const invited = room.getMembersWithMembership("invite").map((m) => m.userId);
line("joined", members.join(", ") || "(none)");
line("invited (pending)", invited.join(", ") || "(none)");

// Who will actually receive the megolm key. If heddle's device is not here, it cannot
// decrypt, and that is a key-sharing problem rather than a wire-format one.
const devices = await crypto.getUserDeviceInfo([INVITE], true);
const theirs = devices.get(INVITE);
line("recipient devices", theirs ? [...theirs.keys()].join(", ") || "(none)" : "(unknown)");

// ── send ─────────────────────────────────────────────────────────────────────
console.log("\nsending (encrypted)");
const KEY = "dev.heddle.agent.v1";
const root = await client.sendEvent(roomId, "m.room.message", {
	msgtype: "m.text",
	body: "heddle phase 0: encrypted thread root",
});
line("root", root.event_id);

const turn = `01SPIKEENC${Date.now().toString(36).toUpperCase()}`;
const session = `spike-enc/${root.event_id}`;
const rel = {
	rel_type: "m.thread",
	event_id: root.event_id,
	is_falling_back: true,
	"m.in_reply_to": { event_id: root.event_id },
};

const call = await client.sendEvent(roomId, "m.room.message", {
	"m.relates_to": rel,
	msgtype: "m.notice",
	body: '🔧 edit: "crates/heddle-agent/src/protocol.rs"',
	[KEY]: {
		v: 1,
		session_id: session,
		turn_id: turn,
		seq: 1,
		kind: "tool.call",
		agent: { name: "opencode", model: "claude-opus-5", version: "spike" },
		tool: {
			name: "edit",
			index: 0,
			args: { filePath: "crates/heddle-agent/src/protocol.rs" },
			preview: "crates/heddle-agent/src/protocol.rs",
			status: "running",
		},
	},
});
line("tool.call", call.event_id);

const result = {
	v: 1,
	session_id: session,
	turn_id: turn,
	seq: 2,
	kind: "tool.result",
	tool: {
		name: "edit",
		index: 0,
		status: "ok",
		duration_ms: 812,
		mime: "text/x-diff",
		body: "--- a/crates/heddle-agent/src/protocol.rs\n+++ b/crates/heddle-agent/src/protocol.rs\n@@ -1,3 +1,4 @@\n+// proved through megolm\n",
		truncated: false,
	},
};
const edit = await client.sendEvent(roomId, "m.room.message", {
	"m.relates_to": { rel_type: "m.replace", event_id: call.event_id },
	msgtype: "m.notice",
	body: '* 🔧 edit ✓ 0.8s',
	"m.new_content": { msgtype: "m.notice", body: "🔧 edit ✓ 0.8s", [KEY]: result },
	[KEY]: result,
});
line("edit", edit.event_id);

// ── verify: encrypted in transit, structured after decryption ────────────────
console.log("\nverification");
await new Promise((r) => setTimeout(r, 3000));
const raw = await client.fetchRoomEvent(roomId, call.event_id);
line("type on the wire", raw.type);
line("ciphertext present", raw.content?.ciphertext ? "yes" : "no");

const ev = room.findEventById(call.event_id);
await client.decryptEventIfNeeded(ev);
const clear = ev.getClearContent() ?? ev.getContent();
line("decrypts locally", clear?.[KEY] ? "yes" : "NO");
if (clear?.[KEY]) {
	line("kind", clear[KEY].kind);
	line("tool", clear[KEY].tool?.name);
}

fs.writeFileSync(
	new URL("./last-run.json", import.meta.url).pathname,
	JSON.stringify({ roomId, root: root.event_id, call: call.event_id, edit: edit.event_id }, null, 1),
);

console.log("\ncriteria");
const say = (ok, t) => console.log(`  ${ok ? "PASS" : "BLOCKED"}  ${t}`);
say(Boolean(who.device_id), "device-scoped credential");
say(await crypto.isEncryptionEnabledInRoom(roomId), "room is encrypted");
say(raw.type === "m.room.encrypted", "sent as ciphertext");
say(Boolean(clear?.[KEY]), "dev.heddle.agent.v1 survives megolm");
console.log(`\n  room for heddle: ${roomId}\n`);

await client.stopClient();
process.exit(0);
