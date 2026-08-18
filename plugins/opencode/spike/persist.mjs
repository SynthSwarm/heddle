// Does the rust-crypto store survive a restart in Node?
//
// This is the phase 0 risk that matters. Node ships no IndexedDB, which is the only
// persistent store matrix-js-sdk's rust crypto knows how to use. If the store cannot
// persist, every restart regenerates device keys under a device ID the server already
// has keys for -- and the symptom is other people's clients failing to decrypt, which
// is a miserable thing to debug later.
//
// Run twice. The second run must report the same identity keys as the first.

import * as sdk from "matrix-js-sdk";

const HOMESERVER = process.env.MATRIX_HOME_SERVER;
const TOKEN = process.env.MATRIX_ACCESS_TOKEN;
const STORE = new URL("./store/", import.meta.url).pathname;

const mode = process.argv[2] ?? "none"; // none | fake | node-indexeddb

if (mode === "fake") {
	const fake = await import("fake-indexeddb");
	globalThis.indexedDB = fake.indexedDB;
	globalThis.IDBKeyRange = fake.IDBKeyRange;
} else if (mode === "node-indexeddb") {
	// Order matters and the failure is not obvious: the LevelDB cache must be loaded
	// before the auto module is imported, or the factory constructs itself against an
	// unloaded database, logs "Database not loaded yet" and silently gives you a store
	// that starts empty every time -- which looks exactly like success.
	const { default: dbManager } = await import("node-indexeddb/dbManager");
	await dbManager.loadCache();
	await import("node-indexeddb/auto");
}

console.log(`\nmode: ${mode}`);
console.log(`indexedDB present: ${typeof globalThis.indexedDB !== "undefined"}`);

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

try {
	await client.initRustCrypto({ useIndexedDB: mode !== "none", cryptoDatabasePrefix: STORE });
	console.log("initRustCrypto: ok");
} catch (e) {
	console.log(`initRustCrypto FAILED: ${e.message.slice(0, 120)}`);
	process.exit(1);
}

const crypto = client.getCrypto();
const keys = await crypto.getOwnDeviceKeys();
console.log(`device:      ${who.device_id}`);
console.log(`curve25519:  ${keys.curve25519}`);
console.log(`ed25519:     ${keys.ed25519}`);
console.log("\n^ run again: identical keys mean the store persisted.\n");
process.exit(0);
