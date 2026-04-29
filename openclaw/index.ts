/**
 * Enzyme — OpenClaw memory/search plugin.
 *
 * Wraps the `enzyme` CLI binary (https://enzyme.garden) and exposes:
 *   - Tools: enzyme_petri, enzyme_catalyze, enzyme_status
 *   - Hook (optional): before_prompt_build — auto-inject petri context per turn
 *   - Hook (optional): agent_end — refresh the vault index after a turn
 *
 * Process spawning lives in @jshph/enzyme-openclaw-bridge so this entry stays
 * free of direct child_process imports (skips the unsafe-install prompt).
 */

// @ts-ignore - resolved by openclaw runtime
import { definePluginEntry } from "openclaw/plugin-sdk/plugin-entry";
import {
  runEnzyme,
  summarizePetri,
  type EnzymeBridgeConfig,
} from "@jshph/enzyme-openclaw-bridge";

type EnzymeConfig = EnzymeBridgeConfig & {
  autoRecall?: boolean;
  autoRefresh?: boolean;
};

export default definePluginEntry({
  id: "enzyme",
  name: "Enzyme",
  description: "Petri working memory + catalyze concept search over Obsidian vaults",
  register(api: any) {
    const getCfg = (): EnzymeConfig => (api?.config ?? {}) as EnzymeConfig;

    // ---- Tools ----------------------------------------------------------

    api.registerTool({
      name: "enzyme_petri",
      description:
        "Vault working memory. Returns top entities and catalysts (thematic phrases) ranked by relevance to the query, or by recency if no query is given. Use the catalyst vocabulary to compose enzyme_catalyze queries.",
      parameters: {
        type: "object",
        properties: {
          query: {
            type: "string",
            description: "Optional. The user's question or topic. Omit for a recency-based landscape.",
          },
        },
      },
      async execute(_id: string, params: { query?: string }) {
        const cfg = getCfg();
        const args = ["petri", "--json"];
        if (params?.query) args.push("--query", params.query);
        const r = await runEnzyme(args, cfg, { timeoutMs: 15_000 });
        if (r.code !== 0) {
          return {
            content: [{ type: "text", text: `enzyme petri failed: ${r.stderr || r.stdout}` }],
            isError: true,
          };
        }
        return { content: [{ type: "text", text: r.stdout.trim() }] };
      },
    });

    api.registerTool({
      name: "enzyme_catalyze",
      description:
        "Semantic search over the vault by concept/theme. Compose queries with catalyst vocabulary from enzyme_petri rather than the user's raw words.",
      parameters: {
        type: "object",
        required: ["query"],
        properties: {
          query: { type: "string", description: "Concept-style query." },
          register: {
            type: "string",
            enum: ["explore", "continuity", "reference"],
            description: "Presentation register. Default: explore.",
          },
        },
      },
      async execute(_id: string, params: { query: string; register?: string }) {
        const cfg = getCfg();
        const args = ["catalyze", params.query, "--json"];
        if (params?.register) args.push("--register", params.register);
        const r = await runEnzyme(args, cfg, { timeoutMs: 30_000 });
        if (r.code !== 0) {
          return {
            content: [{ type: "text", text: `enzyme catalyze failed: ${r.stderr || r.stdout}` }],
            isError: true,
          };
        }
        return { content: [{ type: "text", text: r.stdout.trim() }] };
      },
    });

    api.registerTool({
      name: "enzyme_status",
      description: "Report vault index stats: doc count, entity count, catalyst coverage.",
      parameters: { type: "object", properties: {} },
      async execute(_id: string) {
        const cfg = getCfg();
        const r = await runEnzyme(["status"], cfg, { timeoutMs: 10_000 });
        return {
          content: [{ type: "text", text: r.stdout.trim() || r.stderr.trim() }],
          isError: r.code !== 0,
        };
      },
    });

    // ---- Hooks ----------------------------------------------------------

    api.on(
      "before_prompt_build",
      async (event: any) => {
        const cfg = (event?.context?.pluginConfig ?? getCfg()) as EnzymeConfig;
        if (cfg.autoRecall === false) return;

        const userPrompt: string =
          event?.prompt ?? event?.userMessage ?? event?.input ?? "";
        if (!userPrompt || userPrompt.length < 3) return;

        const r = await runEnzyme(
          ["petri", "--query", userPrompt, "--json"],
          cfg,
          { timeoutMs: 8_000 },
        );
        if (r.code !== 0) return;
        const text = summarizePetri(r.stdout);
        if (!text) return;
        return {
          prependSystemContext: `## Enzyme — vault working memory\n\n${text}`,
        };
      },
      { priority: 50, timeoutMs: 9_000 },
    );

    api.on(
      "agent_end",
      async (event: any) => {
        const cfg = (event?.context?.pluginConfig ?? getCfg()) as EnzymeConfig;
        if (!cfg.autoRefresh) return;
        await runEnzyme(["refresh", "--quiet"], cfg, { timeoutMs: 20_000 });
      },
      { priority: 10, timeoutMs: 21_000 },
    );
  },
});
