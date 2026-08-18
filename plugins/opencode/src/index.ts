/**
 * heddle-opencode: stream an opencode session to Matrix as structured agent events.
 *
 * heddle can render tool calls, results, durations and diffs losslessly, but only if
 * something puts them on the wire. Until now nothing did -- the client's own SPEC says
 * so -- and every pane fell back to recovering structure from printed text, marked `~`.
 * This is the producer side of `dev.heddle.agent.v1`.
 */

import type { Plugin, Hooks } from "@opencode-ai/plugin";
import { load } from "./config.js";
import { connect, type Transport } from "./matrix.js";
import { Bridge } from "./bridge.js";

const log = (msg: string) => console.log(`[heddle] ${msg}`);

export const HeddleOpencode: Plugin = async ({ client }): Promise<Hooks> => {
	const config = load();
	if (!config || !config.enabled) {
		// Not configured is the normal state for anyone who has installed the plugin and
		// not set it up. Saying nothing is correct; erroring on every start is not.
		return {};
	}

	let transport: Transport | null = null;
	let bridge: Bridge | null = null;
	let starting: Promise<void> | null = null;

	// Connecting lazily keeps a misconfigured bridge from delaying opencode's start, and
	// means an agent session that never produces output never opens a Matrix connection.
	const ensure = async (): Promise<Bridge | null> => {
		if (bridge) return bridge;
		if (!starting) {
			starting = (async () => {
				transport = await connect(config);
				bridge = new Bridge(transport, config, log, {
					// The one place the bridge talks back to opencode.
					respond: async (permission, response) => {
						await client.postSessionIdPermissionsPermissionId({
							path: { id: permission.sessionID, permissionID: permission.id },
							body: { response },
						});
					},
				});
				log(`bridged to ${config.roomId} as ${transport.userId} (${transport.deviceId})`);
			})().catch((e) => {
				log(`bridge unavailable: ${(e as Error).message}`);
				starting = null;
			});
		}
		await starting;
		return bridge;
	};

	return {
		event: async ({ event }) => {
			try {
				switch (event.type) {
					case "message.part.updated": {
						const part = event.properties.part as unknown as {
							sessionID: string;
							messageID: string;
							type: string;
						};
						if (!part?.sessionID) return;
						const b = await ensure();
						await b?.onPart(part as never);
						return;
					}

					case "message.updated": {
						const info = event.properties.info as unknown as {
							id: string;
							sessionID: string;
							role: string;
							time?: { completed?: number };
							modelID?: string;
							cost?: number;
							tokens?: { input?: number; output?: number };
						};
						// Only completed assistant turns matter here; the streaming itself
						// arrives as parts.
						if (info?.role !== "assistant" || !info.time?.completed) return;
						const b = await ensure();
						await b?.onMessageComplete(info.sessionID, info.id, info);
						return;
					}

					case "session.updated": {
						const info = event.properties.info as unknown as { id: string; title?: string };
						if (info?.id && info.title) {
							const b = await ensure();
							b?.setTitle(info.id, info.title);
						}
						return;
					}

					case "session.idle": {
						const b = await ensure();
						await b?.flushAll();
						return;
					}

					case "permission.updated": {
						// The agent has stopped and is waiting on a human. This is the
						// event heddle's whole approval UI was built for and which,
						// until now, nothing produced.
						const permission = event.properties as unknown as {
							id: string;
							type: string;
							sessionID: string;
							messageID?: string;
							title: string;
							metadata?: Record<string, unknown>;
						};
						if (!permission?.id) return;
						const b = await ensure();
						await b?.onPermission(permission);
						return;
					}

					case "permission.replied": {
						const props = event.properties as unknown as { permissionID?: string };
						if (!props?.permissionID) return;
						const b = await ensure();
						await b?.onPermissionReplied(props.permissionID);
						return;
					}

					default:
						return;
				}
			} catch (e) {
				// A bridge fault must never take opencode down with it. The session is the
				// user's work; this is a mirror of it.
				log(`event ${event.type} failed: ${(e as Error).message}`);
			}
		},

		dispose: async () => {
			try {
				await bridge?.flushAll();
				await transport?.close();
			} catch (e) {
				log(`shutdown: ${(e as Error).message}`);
			}
		},
	};
};

export default HeddleOpencode;
