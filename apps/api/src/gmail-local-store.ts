import { execFileSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { closeSync, constants, fsyncSync, fstatSync, lstatSync, openSync, readFileSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { object, type GmailState, type GmailStore } from "./gmail-local.js";

export const GMAIL_DATA_DIR = join(homedir(), "Library", "Application Support", "SHOGUN Gmail Backend");
export const GMAIL_STATE_PATH = join(GMAIL_DATA_DIR, "connection.json");
export function keychainSecret(service: string, account: string): string {
  // Secret is read into memory through a pipe. Never put its value in argv or logs.
  try {
    return execFileSync("/usr/bin/security", ["find-generic-password", "-s", service, "-a", account, "-w"], {
      encoding: "utf8", stdio: ["ignore", "pipe", "ignore"], timeout: 15_000,
    }).trim();
  } catch { throw new Error("gmail_keychain_unavailable_run_setup"); }
}
export class FileGmailStore implements GmailStore {
  constructor(private readonly path = GMAIL_STATE_PATH) {}
  load(): GmailState {
    const dir = lstatSync(join(this.path, ".."));
    if (!dir.isDirectory() || dir.isSymbolicLink() || (dir.mode & 0o077) !== 0 || dir.uid !== process.getuid?.()) throw new Error("unsafe_gmail_state_directory");
    const fd = openSync(this.path, constants.O_RDONLY | constants.O_NOFOLLOW);
    try {
      const stat = fstatSync(fd);
      if (!stat.isFile() || stat.size > 4096 || (stat.mode & 0o077) !== 0 || stat.uid !== process.getuid?.()) throw new Error("unsafe_gmail_state_file");
      const data = object(JSON.parse(readFileSync(fd, "utf8")));
      if (Object.keys(data).some((key) => !["userId", "authConfigId", "connectedAccountId", "consentGrantedAt", "lastVerifiedAt", "toolVersion", "lastReadLogId"].includes(key))) throw new Error("invalid_gmail_state");
      if (data.lastReadLogId !== undefined && data.lastReadLogId !== null &&
          (typeof data.lastReadLogId !== "string" || !/^log_[A-Za-z0-9_-]{1,150}$/.test(data.lastReadLogId))) throw new Error("invalid_gmail_read_log_id");
      if (data.toolVersion !== null && (typeof data.toolVersion !== "string" || !/^\d{8}_\d{2}$/.test(data.toolVersion))) throw new Error("invalid_gmail_tool_version");
      for (const key of ["userId", "authConfigId"]) {
        if (typeof data[key] !== "string" || !/^[A-Za-z0-9_-]{1,160}$/.test(data[key] as string)) throw new Error("invalid_gmail_state");
      }
      if (!(data.authConfigId as string).startsWith("ac_")) throw new Error("invalid_gmail_auth_config");
      if (data.connectedAccountId !== null && (typeof data.connectedAccountId !== "string" || !/^ca_[A-Za-z0-9_-]{1,150}$/.test(data.connectedAccountId))) throw new Error("invalid_gmail_state");
      for (const key of ["consentGrantedAt", "lastVerifiedAt"]) {
        if (data[key] !== null && (typeof data[key] !== "string" || !Number.isFinite(Date.parse(data[key] as string)))) throw new Error("invalid_gmail_state");
      }
      return data as unknown as GmailState;
    } finally { closeSync(fd); }
  }
  save(state: GmailState): void {
    // Recheck secure directory and current record; writes never include provider responses.
    this.load();
    const temporary = `${this.path}.${randomUUID()}.tmp`;
    try {
      const fd = openSync(temporary, constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | constants.O_NOFOLLOW, 0o600);
      try { writeFileSync(fd, JSON.stringify(state)); fsyncSync(fd); } finally { closeSync(fd); }
      renameSync(temporary, this.path);
      const directory = openSync(join(this.path, ".."), constants.O_RDONLY | constants.O_NOFOLLOW);
      try { fsyncSync(directory); } finally { closeSync(directory); }
    } finally {
      try { unlinkSync(temporary); } catch { /* Already renamed, or never created. */ }
    }
  }
}
