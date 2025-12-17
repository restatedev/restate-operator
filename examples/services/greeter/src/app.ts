import * as restate from "@restatedev/restate-sdk";
import * as http2 from "http2";

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
  },
});

// Startup banner
console.log(`
┌─────────────────────────────────────────────────────┐
│         Restate Example Service                     │
├─────────────────────────────────────────────────────┤
│  Version:   ${VERSION.padEnd(40)}│
│  Pod:       ${POD_NAME.padEnd(40)}│
│  Antidote:  ${(ANTIDOTE || "(not set)").padEnd(40)}│
│  Port:      9080                                    │
├─────────────────────────────────────────────────────┤
│  Endpoints:                                         │
│    POST /Greeter/greet      - Quick greeting        │
│    POST /Greeter/slowGreet  - Delayed greeting      │
└─────────────────────────────────────────────────────┘
`);

// Create HTTP2 server with manual lifecycle control for graceful shutdown
const handler = restate.createEndpointHandler({ services: [greeter] });
const server = http2.createServer(handler);

const PORT = 9080;
server.listen(PORT, () => {
  console.log(`[${VERSION}/${POD_NAME}] Server listening on port ${PORT}`);
});

// Graceful shutdown on SIGTERM (kubelet) and SIGINT (Ctrl+C)
const shutdown = (signal: string) => {
  console.log(`[${VERSION}/${POD_NAME}] Received ${signal}, shutting down gracefully...`);

  server.close(() => {
    console.log(`[${VERSION}/${POD_NAME}] Server closed, exiting.`);
    process.exit(0);
  });

  // Force exit after 30s if graceful shutdown hangs
  setTimeout(() => {
    console.error(`[${VERSION}/${POD_NAME}] Graceful shutdown timeout, forcing exit.`);
    process.exit(1);
  }, 30000);
};

process.on("SIGTERM", () => shutdown("SIGTERM"));
process.on("SIGINT", () => shutdown("SIGINT"));
