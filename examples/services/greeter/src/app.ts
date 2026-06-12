import * as restate from "@restatedev/restate-sdk";

// Deployment identity from environment
const VERSION = process.env.SERVICE_VERSION || "v1";
const POD_NAME = process.env.POD_NAME || process.env.HOSTNAME || "local";

// Antidote configuration - set this env var to handle "poison" inputs
const ANTIDOTE = process.env.ANTIDOTE || "";

const greeter = restate.service({
  name: "Greeter",
  handlers: {
    /**
     * Simple greet - returns immediately with version info.
     *
     * If the name is "poison" and ANTIDOTE env var is not set,
     * the handler will throw an error. This demonstrates fixing
     * a deployment in-place by updating an environment variable.
     *
     * Usage:
     *   curl localhost:8080/Greeter/greet -d '"Alice"'    # works
     *   curl localhost:8080/Greeter/greet -d '"poison"'   # fails without ANTIDOTE
     *   # Update deployment with ANTIDOTE=cure, then:
     *   curl localhost:8080/Greeter/greet -d '"poison"'   # works
     */
    greet: async (ctx: restate.Context, name: string): Promise<object> => {
      // Check for poison input
      if (name.toLowerCase() === "poison") {
        if (!ANTIDOTE) {
          console.log(`[${VERSION}] Received poison input but no ANTIDOTE configured!`);
          throw new Error(
            "Temporarily poisoned! Restate will retry. Set ANTIDOTE environment variable to fix."
          );
        }
        console.log(`[${VERSION}] Poison neutralized with antidote: ${ANTIDOTE}`);
        return {
          message: `Hello ${name}! (neutralized with: ${ANTIDOTE})`,
          version: VERSION,
          pod: POD_NAME,
          antidote: ANTIDOTE,
          timestamp: new Date().toISOString(),
        };
      }

      return {
        message: `Hello ${name}!`,
        version: VERSION,
        pod: POD_NAME,
        timestamp: new Date().toISOString(),
      };
    },

    /**
     * Slow greet - takes configurable time to complete.
     * Useful for demonstrating graceful draining during deployments.
     *
     * Usage:
     *   # Start a slow request
     *   curl localhost:8080/Greeter/slowGreet -d '{"name":"Alice","delaySeconds":60}'
     *
     *   # While running, update the deployment
     *   # Watch the old pod stay alive until this request completes
     */
    slowGreet: async (
      ctx: restate.Context,
      request: { name: string; delaySeconds?: number }
    ): Promise<object> => {
      const { name, delaySeconds = 30 } = request;

      // Check for poison
      if (name.toLowerCase() === "poison" && !ANTIDOTE) {
        throw new Error(
          "Temporarily poisoned! Restate will retry. Set ANTIDOTE environment variable to fix."
        );
      }

      console.log(`[${VERSION}/${POD_NAME}] Starting ${delaySeconds}s greeting for ${name}`);

      // Durable sleep - survives crashes and restarts
      await ctx.sleep(delaySeconds * 1000);

      console.log(`[${VERSION}/${POD_NAME}] Completed greeting for ${name}`);

      const response: Record<string, unknown> = {
        message: `Hello ${name}! (after ${delaySeconds}s)`,
        version: VERSION,
        pod: POD_NAME,
        delaySeconds,
        timestamp: new Date().toISOString(),
      };

      if (name.toLowerCase() === "poison") {
        response.antidote = ANTIDOTE;
      }

      return response;
    },

    /**
     * CPU burn - keeps a pod busy for `durationSeconds`, used to demonstrate
     * HorizontalPodAutoscaler scale-up/down of a *draining* version. Start
     * several of these one-way (`/Greeter/burn/send`) against a version BEFORE
     * bumping to a newer version: they stay pinned to (and executing on) the old
     * version, driving its CPU up so the operator-managed HPA scales it beyond
     * the floor, then back down once they finish.
     *
     * Burns CPU in short slices, yielding to the event loop between them so the
     * HTTP/2 connection stays responsive, and journals a periodic tick via
     * ctx.run so Restate sees progress and does not hit the inactivity timeout.
     *
     * Usage:
     *   curl localhost:8080/Greeter/burn/send -d '{"durationSeconds":180}'
     */
    burn: async (
      ctx: restate.Context,
      request: { durationSeconds?: number; tickSeconds?: number; startDelaySeconds?: number }
    ): Promise<object> => {
      const { durationSeconds = 120, tickSeconds = 4, startDelaySeconds = 0 } = request;

      // Optionally stay pinned but idle (durable sleep, no CPU) before burning.
      // Lets a demo observe the version sitting at its HPA floor first, then
      // scale up when the burn kicks in.
      if (startDelaySeconds > 0) {
        console.log(`[${VERSION}/${POD_NAME}] burn idle for ${startDelaySeconds}s before load`);
        await ctx.sleep(startDelaySeconds * 1000);
      }

      const end = Date.now() + durationSeconds * 1000;
      let round = 0;

      console.log(`[${VERSION}/${POD_NAME}] burn start for ${durationSeconds}s`);

      while (Date.now() < end) {
        const tickEnd = Math.min(end, Date.now() + tickSeconds * 1000);
        // Busy-loop in ~20ms slices, yielding between them.
        while (Date.now() < tickEnd) {
          const sliceEnd = Date.now() + 20;
          let x = 0;
          while (Date.now() < sliceEnd) {
            x += Math.sqrt(Math.random() * 1e9);
          }
          if (x < 0) console.log(x); // keep x from being optimized away
          await new Promise((resolve) => setImmediate(resolve));
        }
        // Journal progress so Restate registers activity (no suspension).
        round += 1;
        await ctx.run(`tick-${round}`, async () => round);
      }

      console.log(`[${VERSION}/${POD_NAME}] burn done after ${durationSeconds}s (${round} ticks)`);

      return {
        message: `Burned ${durationSeconds}s`,
        version: VERSION,
        pod: POD_NAME,
        durationSeconds,
        ticks: round,
      };
    },
  },
});

// Startup banner
console.log(`
┌───────────────────────────────────────────────────────────────────────────┐
│         Restate Example Service                                           │
├───────────────────────────────────────────────────────────────────────────┤
│  Version:   ${VERSION.padEnd(62)}│
│  Pod:       ${POD_NAME.padEnd(62)}│
│  Antidote:  ${(ANTIDOTE || "(not set)").padEnd(62)}│
│  Port:      9080                                                          │
├───────────────────────────────────────────────────────────────────────────┤
│  Endpoints:                                                               │
│    POST /Greeter/greet      - Quick greeting                              │
│    POST /Greeter/slowGreet  - Delayed greeting                            │
└───────────────────────────────────────────────────────────────────────────┘
`);

restate.serve({
  services: [greeter],
  port: 9080,
});
