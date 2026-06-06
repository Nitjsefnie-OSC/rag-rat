export function openDatabase(): void {
  NativeHeldCore.openDatabase();
}

export type BridgeState = "open" | "closed";

export const bridgeName = "held-mini";

export interface BridgeConfig {
  readonly name: string;
}

export class BridgeClient {
  open(): void {}
}

export const useBridge = () => bridgeName;

export const BridgeBadge = function BridgeBadge() {
  return bridgeName;
};
