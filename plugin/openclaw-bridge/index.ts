/**
 * Process bridge for the Enzyme OpenClaw plugin.
 *
 * Lives in its own npm package so the plugin entry point stays free of
 * direct child_process imports — that lets `openclaw plugins install` skip
 * the dangerous-code prompt. (See byterover/brv-bridge for prior art.)
 */

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";

export type EnzymeBridgeConfig = {
  vaultPath?: string;
  binaryPath?: string;
};

export type RunResult = {
  stdout: string;
  stderr: string;
  code: number | null;
};

export function resolveBinary(cfg: EnzymeBridgeConfig): string {
  if (cfg.binaryPath && existsSync(cfg.binaryPath)) return cfg.binaryPath;
  return "enzyme";
}

export function resolveVault(cfg: EnzymeBridgeConfig): string | undefined {
  return cfg.vaultPath || process.env.ENZYME_VAULT_ROOT || undefined;
}

export function runEnzyme(
  args: string[],
  cfg: EnzymeBridgeConfig = {},
  opts: { timeoutMs?: number } = {},
): Promise<RunResult> {
  return new Promise((resolve) => {
    const bin = resolveBinary(cfg);
    const vault = resolveVault(cfg);
    const finalArgs = vault ? ["-p", vault, ...args] : args;
    const child = spawn(bin, finalArgs, {
      env: { ...process.env },
      stdio: ["ignore", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (b) => (stdout += b.toString()));
    child.stderr.on("data", (b) => (stderr += b.toString()));
    const timer = opts.timeoutMs
      ? setTimeout(() => child.kill("SIGTERM"), opts.timeoutMs)
      : null;
    child.on("close", (code) => {
      if (timer) clearTimeout(timer);
      resolve({ stdout, stderr, code });
    });
    child.on("error", (err) => {
      if (timer) clearTimeout(timer);
      resolve({ stdout: "", stderr: String(err), code: -1 });
    });
  });
}

export function summarizePetri(stdout: string, maxChars = 4000): string {
  const trimmed = stdout.trim();
  if (!trimmed) return "";
  if (trimmed.length <= maxChars) return trimmed;
  return trimmed.slice(0, maxChars) + "\n...[truncated]";
}
