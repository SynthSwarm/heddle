// Generates the conformance fixtures.
//
// The point is that these are *recordings of the emitter*, not hand-written JSON. A
// fixture somebody typed out proves only that they read the schema the same way twice;
// this proves what the code actually puts on the wire, and `cargo test` then proves
// heddle can read it.
//
// No network and no crypto: the transport is replaced by a recorder, so this runs in CI
// in a second and cannot be flaky.
//
//   node test/fixtures.mjs           # write fixtures
//   node test/fixtures.mjs --check   # fail if they have drifted

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { Bridge } from "../dist/bridge.js";

const DIR = path.join(path.dirname(fileURLToPath(import.meta.url)), "fixtures");
const check = process.argv.includes("--check");

// ── a transport that records instead of sending ──────────────────────────────
const recorded = [];
let nextId = 0;
let reactionHandler = () => {};
const transport = {
	userId: "@mason:matrix.example.org",
	deviceId: "FIXTUREDEV",
	async openThread() {
		return "$thread-root";
	},
	async send(_room, threadRoot, body, event) {
		const id = `$event-${++nextId}`;
		recorded.push({ op: "send", eventId: id, threadRoot, body, event });
		return id;
	},
	async edit(_room, target, body, event) {
		recorded.push({ op: "edit", eventId: target, body, event });
	},
	onReaction(handler) {
		reactionHandler = handler;
	},
	async close() {},
};

const config = {
	homeserver: "https://matrix.example.org",
	accessToken: "unused",
	roomId: "!fixtures:matrix.example.org",
	storePath: "/tmp/unused",
	agentName: "opencode",
	// Zero throttling: fixtures are about payload shape, not timing.
	editIntervalMs: 0,
	bufferChars: 0,
	commentary: true,
	enabled: true,
};

let turnCounter = 0;
const answered = [];
const bridge = new Bridge(transport, config, () => {}, {
	newTurnId: () => `01FIXTURETURN${String(++turnCounter).padStart(3, "0")}`,
	respond: async (permission, response) => {
		answered.push({ id: permission.id, response });
	},
});

const SESSION = "ses_fixture";
const MESSAGE = "msg_fixture";
const part = (over) => ({ sessionID: SESSION, messageID: MESSAGE, ...over });
const settle = () => new Promise((r) => setTimeout(r, 5));

bridge.setTitle(SESSION, "conformance fixtures");

// ── a turn that exercises every kind the emitter can produce ─────────────────

// Streaming assistant text, cumulative, on the edit chain.
for (const text of ["The emitter", "The emitter is live", "The emitter is live and lossless."]) {
	await bridge.onPart(part({ id: "p-text", type: "text", text }));
	await settle();
}

// Reasoning -> commentary.
await bridge.onPart(
	part({ id: "p-reason", type: "reasoning", text: "Checking that seq stays contiguous." }),
);
await settle();

// A tool that succeeds, with a diff result.
const editArgs = { filePath: "crates/heddle-agent/src/store.rs" };
await bridge.onPart(
	part({
		id: "p-edit",
		type: "tool",
		tool: "edit",
		state: {
			status: "running",
			input: editArgs,
			title: "crates/heddle-agent/src/store.rs",
			time: { start: 1_000_000 },
		},
	}),
);
await settle();
await bridge.onPart(
	part({
		id: "p-edit",
		type: "tool",
		tool: "edit",
		state: {
			status: "completed",
			input: editArgs,
			title: "crates/heddle-agent/src/store.rs",
			output:
				"--- a/crates/heddle-agent/src/store.rs\n" +
				"+++ b/crates/heddle-agent/src/store.rs\n" +
				"@@ -283,7 +283,7 @@\n" +
				"-        if ev.seq <= turn.high_seq {\n" +
				"+        if ev.seq <= turn.high_seq && !completes_tool {\n",
			time: { start: 1_000_000, end: 1_001_240 },
		},
	}),
);
await settle();

// A tool that fails. Plain text result, so the renderer folds rather than diffs it.
await bridge.onPart(
	part({
		id: "p-bash",
		type: "tool",
		tool: "bash",
		state: {
			status: "running",
			input: { command: "cargo test --workspace" },
			time: { start: 2_000_000 },
		},
	}),
);
await settle();
await bridge.onPart(
	part({
		id: "p-bash",
		type: "tool",
		tool: "bash",
		state: {
			status: "error",
			input: { command: "cargo test --workspace" },
			error: "test result: FAILED. 1 failed; 391 passed",
			time: { start: 2_000_000, end: 2_004_210 },
		},
	}),
);
await settle();

// A JSON result, which selects a different renderer again.
await bridge.onPart(
	part({
		id: "p-json",
		type: "tool",
		tool: "webfetch",
		state: {
			status: "completed",
			input: { url: "https://example.org/versions" },
			output: '{"versions":["v1.11","v1.12"],"unstable":{"org.matrix.simplified_msc3575":true}}',
			time: { start: 3_000_000, end: 3_000_310 },
		},
	}),
);
await settle();

// An approval: the agent stops and waits for a human, who answers with a reaction.
await bridge.onPermission({
	id: "perm_fixture",
	type: "bash",
	sessionID: SESSION,
	messageID: MESSAGE,
	title: "rm -rf ./target",
	metadata: { command: "rm -rf ./target", cwd: "/home/you/heddle" },
});
await settle();

// The reaction heddle sends for `y`. Delivered through the transport's own callback, so
// the path exercised here is the one a real reaction takes.
const askEvent = recorded.findLast((r) => r.event.kind === "approval.request");
reactionHandler({ targetId: askEvent.eventId, key: "✅", sender: "@you:matrix.example.org" });
await settle();
if (answered.length !== 1 || answered[0].response !== "once") {
	console.error("the approval was not passed back to opencode:", answered);
	process.exit(1);
}

// Close the turn: usage then message.stop.
await bridge.onMessageComplete(SESSION, MESSAGE, {
	modelID: "claude-opus-5",
	cost: 0.0412,
	tokens: { input: 18422, output: 970 },
});
await bridge.flushAll();
await settle();

// ── write, or check ──────────────────────────────────────────────────────────

// Every frame is a fixture, because every frame is a payload heddle may be asked to
// decode: a live reader sees the `tool.call` before the edit that completes it, while a
// reader loading the room fresh sees only the resolved result. Both must decode.
//
// The manifest additionally records the *resolved* view -- what survives once edits are
// applied -- because that is the sequence whose `seq` contiguity decides whether a pane
// is marked as missing events.
const resolved = new Map();
const order = [];
for (const r of recorded) {
	if (!resolved.has(r.eventId)) order.push(r.eventId);
	resolved.set(r.eventId, r);
}

const nameOf = (i, ev) => `${String(i).padStart(2, "0")}-${ev.kind.replace(/\./g, "-")}.json`;

const files = new Map();
recorded.forEach((r, i) => {
	files.set(nameOf(i, r.event), `${JSON.stringify(r.event, null, "\t")}\n`);
});

files.set(
	"manifest.json",
	`${JSON.stringify(
		{
			description:
				"Recorded from the heddle-opencode emitter. Regenerate with `npm run fixtures`.",
			frames: recorded.map((r, i) => ({
				file: nameOf(i, r.event),
				op: r.op,
				kind: r.event.kind,
				seq: r.event.seq,
			})),
			resolved: order.map((id) => ({
				kind: resolved.get(id).event.kind,
				seq: resolved.get(id).event.seq,
			})),
		},
		null,
		"\t",
	)}\n`,
);

if (check) {
	let drift = 0;
	const onDisk = new Set(fs.existsSync(DIR) ? fs.readdirSync(DIR) : []);
	for (const [name, body] of files) {
		onDisk.delete(name);
		const p = path.join(DIR, name);
		if (!fs.existsSync(p)) {
			console.error(`missing fixture: ${name}`);
			drift++;
		} else if (fs.readFileSync(p, "utf8") !== body) {
			console.error(`fixture differs: ${name}`);
			drift++;
		}
	}
	for (const stale of onDisk) {
		console.error(`stale fixture: ${stale}`);
		drift++;
	}
	if (drift > 0) {
		console.error(`\n${drift} fixture(s) out of date. Run: npm run fixtures`);
		process.exit(1);
	}
	console.log(`fixtures up to date (${files.size - 1} frames)`);
} else {
	fs.rmSync(DIR, { recursive: true, force: true });
	fs.mkdirSync(DIR, { recursive: true });
	for (const [name, body] of files) fs.writeFileSync(path.join(DIR, name), body);
	console.log(`wrote ${files.size - 1} frames + manifest to ${DIR}`);
	console.log("\nresolved view (what a fresh reader sees):");
	for (const id of order) {
		const e = resolved.get(id).event;
		console.log(`  seq ${String(e.seq).padStart(2)}  ${e.kind}`);
	}
}
