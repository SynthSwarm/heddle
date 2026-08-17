// Reads back what the bridge emitted, decrypted, and checks the two properties that
// decide whether heddle renders a clean pane:
//
//   1. every payload decodes as dev.heddle.agent.v1
//   2. seq is contiguous per turn in the *resolved* view -- which is all heddle sees,
//      and a hole in it is the `!` marker on every pane
//
//   node test/verify.mjs

import fs from "node:fs";
import { load } from "../dist/config.js";
import { connect } from "../dist/matrix.js";
import * as sdk from "matrix-js-sdk";

const KEY = "dev.heddle.agent.v1";
const OUT = "/tmp/p1";

const config = load();
const transport = await connect(config);
// connect() already synced; reach the underlying client through a fresh one is overkill,
// so re-create with the same store and pull the timeline.
const client = sdk.createClient({
	baseUrl: config.homeserver,
	accessToken: config.accessToken,
	userId: transport.userId,
	deviceId: transport.deviceId,
});
await client.initRustCrypto({
	useIndexedDB: true,
	cryptoDatabasePrefix: `${config.storePath}/store/`,
});
await client.startClient({ initialSyncLimit: 100 });
await new Promise((res, rej) => {
	const t = setTimeout(() => rej(new Error("sync timeout")), 60000);
	client.once(sdk.ClientEvent.Sync, (s) => s === "PREPARED" && (clearTimeout(t), res()));
});

const room = client.getRoom(config.roomId);
await client.scrollback(room, 100);

fs.rmSync(OUT, { recursive: true, force: true });
fs.mkdirSync(OUT, { recursive: true });

const payloads = [];
for (const ev of room.getLiveTimeline().getEvents()) {
	await client.decryptEventIfNeeded(ev);
	const c = ev.getClearContent() ?? ev.getContent();
	// Resolved view: an edit replaces its target, which is exactly what heddle reads.
	const replaced = ev.replacingEvent();
	const eff = replaced ? (replaced.getClearContent() ?? replaced.getContent()) : c;
	const p = eff?.["m.new_content"]?.[KEY] ?? eff?.[KEY];
	if (p) payloads.push(p);
}

const turns = new Map();
payloads.forEach((p, i) => {
	fs.writeFileSync(`${OUT}/${String(i).padStart(2, "0")}-${p.kind.replace(/\./g, "_")}.json`, JSON.stringify(p, null, 1));
	if (!turns.has(p.turn_id)) turns.set(p.turn_id, []);
	turns.get(p.turn_id).push(p);
});

console.log(`\n${payloads.length} payloads across ${turns.size} turn(s)\n`);
let bad = 0;
for (const [tid, ps] of turns) {
	console.log(`turn ${tid.slice(0, 14)}…`);
	for (const p of ps) {
		const t = p.tool ?? {};
		let extra = "";
		if (p.kind.startsWith("tool")) {
			extra = ` idx=${t.index} ${t.name} ${t.status}${t.duration_ms ? ` ${t.duration_ms}ms` : ""}${t.mime ? ` ${t.mime}` : ""}`;
		}
		if (p.kind === "usage") extra = ` ${JSON.stringify(p.usage)}`;
		if (p.kind === "message.delta") extra = ` ${JSON.stringify((p.text ?? "").slice(0, 44))}…`;
		console.log(`  seq ${String(p.seq).padStart(2)}  ${p.kind.padEnd(14)}${extra}`);
	}
	const seqs = [...new Set(ps.map((p) => p.seq))].sort((a, b) => a - b);
	const gaps = [];
	for (let n = seqs[0]; n <= seqs[seqs.length - 1]; n++) if (!seqs.includes(n)) gaps.push(n);
	console.log(`  seqs ${JSON.stringify(seqs)}  gaps ${gaps.length ? JSON.stringify(gaps) : "NONE"}\n`);
	if (gaps.length) bad++;
}

console.log(bad === 0 ? "no seq gaps: heddle will not mark these panes !" : `${bad} turn(s) with gaps`);
client.stopClient();
await transport.close();
process.exit(bad === 0 ? 0 : 1);
