// Drives the real Bridge with the event shapes opencode emits, against the real room.
//
// Not a unit test: the point is to exercise transport, crypto, threading, the edit chain
// and seq allocation together, because every bug found in phase 0 lived in the seams
// between those rather than inside any one of them.
//
//   node test/live.mjs

import { load } from "../dist/config.js";
import { connect } from "../dist/matrix.js";
import { Bridge } from "../dist/bridge.js";

const config = load();
if (!config) {
	console.error("not configured: export MATRIX_HOME_SERVER, MATRIX_ACCESS_TOKEN, HEDDLE_MATRIX_ROOM");
	process.exit(2);
}
// Fast enough to watch, slow enough to still exercise throttling.
config.editIntervalMs = 400;

const log = (m) => console.log(`[heddle] ${m}`);
const transport = await connect(config);
log(`connected as ${transport.userId} (${transport.deviceId})`);

// A tiny indirection so the live script can install its own responder after
// construction, which is when it knows what to print.
const bridgeRespond = { handler: async () => {} };
const bridge = new Bridge(transport, config, log, {
	respond: (permission, response) => bridgeRespond.handler(permission, response),
});

const SESSION = `live-${Date.now().toString(36)}`;
const MESSAGE = `msg-${Date.now().toString(36)}`;
bridge.setTitle(SESSION, "heddle-opencode: phase 1");

const part = (over) => ({ sessionID: SESSION, messageID: MESSAGE, ...over });
const wait = (ms) => new Promise((r) => setTimeout(r, ms));

console.log("\nstreaming assistant text (cumulative, edit chain)");
const full =
	"Right — the emitter is live. Tool calls carry their arguments, results carry " +
	"durations and diffs, and none of it is recovered from printed chrome.";
for (let i = 12; i <= full.length; i += 12) {
	await bridge.onPart(part({ id: "part-text", type: "text", text: full.slice(0, i) }));
	await wait(120);
}
await bridge.onPart(part({ id: "part-text", type: "text", text: full }));
await wait(700);

console.log("reasoning -> commentary");
await bridge.onPart(
	part({ id: "part-reason", type: "reasoning", text: "Checking the seq allocation holds." }),
);

console.log("tool call -> running");
const started = Date.now();
await bridge.onPart(
	part({
		id: "part-tool-1",
		type: "tool",
		tool: "edit",
		state: {
			status: "running",
			input: { filePath: "crates/heddle-agent/src/store.rs" },
			title: "crates/heddle-agent/src/store.rs",
			time: { start: started },
		},
	}),
);
await wait(900);

console.log("tool result -> ok, with a diff");
await bridge.onPart(
	part({
		id: "part-tool-1",
		type: "tool",
		tool: "edit",
		state: {
			status: "completed",
			input: { filePath: "crates/heddle-agent/src/store.rs" },
			title: "crates/heddle-agent/src/store.rs",
			output:
				"--- a/crates/heddle-agent/src/store.rs\n" +
				"+++ b/crates/heddle-agent/src/store.rs\n" +
				"@@ -284,6 +284,7 @@\n" +
				"     let fills_gap = turn.gaps.contains(&ev.seq);\n" +
				"+    // one seq per matrix event, never per edit\n" +
				"     if ev.seq <= turn.high_seq {\n",
			time: { start: started, end: started + 1240 },
		},
	}),
);

console.log("a second tool, failing");
const t2 = Date.now();
await bridge.onPart(
	part({
		id: "part-tool-2",
		type: "tool",
		tool: "bash",
		state: {
			status: "running",
			input: { command: "cargo test --workspace" },
			time: { start: t2 },
		},
	}),
);
await wait(400);
await bridge.onPart(
	part({
		id: "part-tool-2",
		type: "tool",
		tool: "bash",
		state: {
			status: "error",
			input: { command: "cargo test --workspace" },
			error: "test result: FAILED. 1 failed",
			time: { start: t2, end: t2 + 4210 },
		},
	}),
);

console.log("asking for approval — answer it in heddle with y or n");
let answered = null;
bridgeRespond.handler = async (permission, response) => {
	answered = { title: permission.title, response };
	console.log(`  opencode was told: ${response}`);
};
await bridge.onPermission({
	id: `perm-${Date.now().toString(36)}`,
	type: "bash",
	sessionID: SESSION,
	messageID: MESSAGE,
	title: "rm -rf ./target",
	metadata: { command: "rm -rf ./target", cwd: process.cwd() },
});

// Wait for a human. This is the point of the exercise.
const deadline = Date.now() + 120_000;
while (!answered && Date.now() < deadline) await wait(1000);
if (!answered) console.log("  nobody answered within two minutes");

console.log("completing the turn (usage + message.stop)");
await bridge.onMessageComplete(SESSION, MESSAGE, {
	modelID: "claude-opus-5",
	cost: 0.0412,
	tokens: { input: 18422, output: 970 },
});

await bridge.flushAll();
await transport.close();
console.log("\ndone — look at the thread in heddle\n");
process.exit(0);
