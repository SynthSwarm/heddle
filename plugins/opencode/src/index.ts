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

export const HeddleOpencode: Plugin = async ({ client }): Promise<Hooks> => {
	// Never `console.log`. Inside a plugin that is opencode's TUI, and a background
	// bridge printing over the user's terminal is not a diagnostic, it is damage. The
	// SDK's own logging is silenced in `connect` for the same reason.
	const log = (msg: string) => {
		void client?.app
			?.log({ body: { service: "heddle", level: "info", message: msg } })
			.catch(() => {});
	};

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
				// The agent has stopped and is waiting on a human. This is the event
				// heddle's whole approval UI was built for and which, until now, nothing
				// produced.
				//
				// Two names, deliberately. `@opencode-ai/sdk` types the event as
				// `permission.updated`, while `@opencode-ai/sdk/v2` and the plugin
				// documentation call it `permission.asked`. Matching only the one the
				// types expose would leave approvals silently never firing on a runtime
				// that emits the other, which is the worst shape of bug: the feature
				// looks present and does nothing.
				const onPermission = async (props: unknown) => {
					const permission = props as {
						id?: string;
						type?: string;
						sessionID?: string;
						messageID?: string;
						title?: string;
						metadata?: Record<string, unknown>;
					};
					if (!permission?.id || !permission.sessionID) return;
					const b = await ensure();
					await b?.onPermission({
						id: permission.id,
						type: permission.type ?? "tool",
						sessionID: permission.sessionID,
						messageID: permission.messageID,
						title: permission.title ?? permission.type ?? "permission",
						metadata: permission.metadata,
					});
				};

				// Matched as a string, because `permission.asked` is not in the union the
				// installed types declare.
				if ((event.type as string) === "permission.asked") {
					await onPermission((event as { properties?: unknown }).properties);
					return;
				}

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
						await onPermission(event.properties);
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
