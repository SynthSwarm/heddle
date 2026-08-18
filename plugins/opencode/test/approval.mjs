// Sends one approval into the room and waits for a human to answer it in heddle.
//
// The last leg that no test covers: heddle's `y`/`n` -> m.reaction -> this plugin ->
// the callback opencode would be given. Everything either side of that is tested; the
// middle is only ever driven from the fixture generator, which is not the same as a
// person pressing a key.
//
//   node test/approval.mjs "rm -rf ./target"

import { load } from "../dist/config.js";
import { connect } from "../dist/matrix.js";
import { Bridge } from "../dist/bridge.js";

const title = process.argv[2] ?? "rm -rf ./target";
const minutes = Number(process.env.APPROVAL_WAIT_MINUTES ?? 5);

const config = load();
if (!config) {
	console.error("not configured");
	process.exit(2);
}

const log = (m) => console.log(`[heddle] ${m}`);
const transport = await connect(config);
log(`connected as ${transport.userId} (${transport.deviceId})`);

let decision = null;
const bridge = new Bridge(transport, config, log, {
	respond: async (permission, response) => {
		decision = { id: permission.id, title: permission.title, response };
	},
});

const SESSION = `approval-${Date.now().toString(36)}`;
bridge.setTitle(SESSION, "approval from opencode");

await bridge.onPermission({
	id: `perm-${Date.now().toString(36)}`,
	type: "bash",
	sessionID: SESSION,
	messageID: `msg-${Date.now().toString(36)}`,
	title,
	metadata: { command: title, cwd: process.cwd() },
});

console.log(`\nasked: "${title}"`);
console.log("answer it in heddle: y approves, n denies. ctrl-c to give up.\n");

const deadline = Date.now() + minutes * 60_000;
while (!decision && Date.now() < deadline) {
	await new Promise((r) => setTimeout(r, 500));
}

if (decision) {
	console.log(`\nANSWERED: ${decision.response}`);
	console.log(`opencode would have been told: response=${decision.response}`);
} else {
	console.log(`\nno answer within ${minutes} minutes`);
}

await transport.close();
process.exit(decision ? 0 : 3);
