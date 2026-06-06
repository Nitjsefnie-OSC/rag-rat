export function openDatabase(): void {
  NativeHeldCore.openDatabase();
}

export type BridgeState = "open" | "closed";

export const bridgeName = "held-mini";
