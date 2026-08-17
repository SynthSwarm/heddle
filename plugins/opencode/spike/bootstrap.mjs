// Give mason a cross-signing identity and self-sign its device.
//
// heddle showed "sent from an unverified device", which is ShieldReason::UnsignedDevice
// -- "the sending device was never signed by its owner". That is not heddle being fussy:
// an agent account with no cross-signing identity is genuinely unattestable, and every
// message it sends is flagged in every client that checks.
//
// Uploading the signing keys is a User-Interactive Auth operation, so this needs the
// password rather than the token.

import * as sdk from "matrix-js-sdk";

const { default: dbManager } = await import("node-indexeddb/dbManager");
await dbManager.loadCache();
await import("node-indexeddb/auto");

const HOMESERVER = process.env.MATRIX_HOME_SERVER;
const TOKEN = process.env.MATRIX_ACCESS_TOKEN;
const PASSWORD = process.env.MATRIX_PASSWORD;
const STORE = new URL("./store/", import.meta.url).pathname;

if (!PASSWORD) {
	console.error("$MATRIX_PASSWORD is required: uploading signing keys is a UIA operation");
	process.exit(2);
}

const line = (l, v) => console.log(`  ${l.padEnd(28)} ${v}`);

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

await client.startClient({ initialSyncLimit: 1 });
await new Promise((resolve, reject) => {
	const t = setTimeout(() => reject(new Error("sync timed out")), 60_000);
	client.once(sdk.ClientEvent.Sync, (s) => {
		if (s === "PREPARED") {
			clearTimeout(t);
			resolve();
		}
	});
});

console.log("\nbefore");
const before = await crypto.getDeviceVerificationStatus(who.user_id, who.device_id);
line("cross-signed by owner", before?.crossSigningVerified ?? false);
line("signed by own device", before?.signedByOwner ?? false);

console.log("\nbootstrapping cross-signing");
await crypto.bootstrapCrossSigning({
	setupNewCrossSigning: false, // do not clobber an identity that already exists
	authUploadDeviceSigningKeys: async (makeRequest) => {
		await makeRequest({
			type: "m.login.password",
			identifier: { type: "m.id.user", user: who.user_id },
			password: PASSWORD,
		});
	},
});
line("done", "signing keys uploaded");

console.log("\nafter");
const status = await crypto.getCrossSigningStatus();
line("master key on server", status?.publicKeysOnDevice ?? "?");
line("private keys cached", JSON.stringify(status?.privateKeysCachedLocally ?? {}));
const after = await crypto.getDeviceVerificationStatus(who.user_id, who.device_id);
line("cross-signed by owner", after?.crossSigningVerified ?? false);
line("signed by own device", after?.signedByOwner ?? false);

const id = await crypto.getUserVerificationStatus(who.user_id);
line("own identity verified", id?.isVerified?.() ?? "n/a");

await client.stopClient();
process.exit(0);
