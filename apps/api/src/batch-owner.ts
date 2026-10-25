export interface BatchOwnerRecord {
  lic: string;
  date: string;
  device?: string | undefined;
  providerId?: string;
}

export interface KeyedBatchOwner {
  licenseId: string;
  deviceId: string;
}

/** A keyed request remains device-bound permanently. Only pre-device legacy batches may
 * use account ownership, and only during their original seven-day recovery window. */
export function resolveBatchOwner(
  owner: BatchOwnerRecord | undefined,
  keyed: KeyedBatchOwner | undefined,
  deviceId?: string,
  now: Date = new Date(),
): string | undefined {
  if (keyed) return deviceId === undefined || keyed.deviceId === deviceId ? keyed.licenseId : undefined;
  if (!owner) return undefined;
  if (owner.device !== undefined) return deviceId === undefined || owner.device === deviceId ? owner.lic : undefined;
  if (!/^\d{4}-\d{2}-\d{2}$/.test(owner.date)) return undefined;
  const date = new Date(`${owner.date}T00:00:00Z`);
  const stamp = date.getTime();
  if (!Number.isFinite(stamp) || date.toISOString().slice(0, 10) !== owner.date) return undefined;
  const today = new Date(`${now.toISOString().slice(0, 10)}T00:00:00Z`).getTime();
  return stamp >= today - 7 * 86_400_000 && stamp <= today ? owner.lic : undefined;
}
