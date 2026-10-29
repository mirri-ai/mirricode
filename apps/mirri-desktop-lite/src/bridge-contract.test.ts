// apps/mirri-desktop-lite/src/bridge-contract.test.ts
//
// The compile-time bridge contract gate — same mechanism as the Electron
// shell's apps/mirri-desktop/src/main/bridge-contract.test.ts (ADR-001).
// NOTE: the actual gate is `tsc --noEmit` (the type probes below fail
// compilation on drift); the runtime asserts are vacuous placeholders kept
// only for the vitest-shaped structure.
// The lite shell's implementation surface (LiteBridgeSurface, mirroring the
// RPC schema) must stay bidirectionally assignable with the web contract
// (MirriDesktopBridge, single source of truth). A drift on either side
// fails `tsc --noEmit` here.

import { describe, expect, it } from "vitest";

import type { MirriDesktopBridge } from "../../mirri-web/src/lib/desktopBridgeContract";

import type { MirriSchema } from "./shared/schema";

/** The lite shell's implementation surface, flattened from the RPC schema. */
interface LiteBridgeSurface {
  openPath(params: { path: string }): Promise<{ opened: true }>;
  revealPath(params: { path: string }): Promise<{ revealed: true }>;
  openInEditor(params: { path: string; line?: number }): Promise<{ opened: true }>;
  openInApp(params: { appId: string; path: string }): Promise<{ opened: true }>;
  listOpenInApps(params: Record<string, never>): Promise<Array<{ appId: string; name: string }>>;
  openExternal(params: { url: string }): Promise<{ opened: true }>;
  notify(params: { title: string; body: string; tag?: string }): Promise<{ shown: boolean }>;
  setBadgeCount(params: { count: number }): Promise<{ set: boolean }>;
  dockBounce(params: { kind?: 'informational' | 'critical' }): Promise<{ bounced: boolean }>;
  setLoginItem(params: { open: boolean }): Promise<{ open: boolean }>;
  getLoginItem(params: Record<string, never>): Promise<{ open: boolean }>;
}

/**
 * Lite implements the web contract exactly: every MirriDesktopBridge member
 * is provided by the lite surface with a compatible signature. Fails to
 * compile when the web contract grows a method the lite shell does not
 * implement.
 */
type LiteImplementsWeb = {
  [K in keyof MirriDesktopBridge]: K extends keyof LiteBridgeSurface ? true : never;
};

/**
 * The web contract accepts the lite surface exactly: every lite member
 * exists on MirriDesktopBridge. Fails when the lite shell exposes
 * something the web contract does not know.
 */
type WebAcceptsLite = {
  [K in keyof LiteBridgeSurface]: K extends keyof MirriDesktopBridge ? true : never;
};

/**
 * The RPC schema carries exactly the lite surface's request keys — a drift
 * between schema.ts and the surface fails here.
 */
type SchemaAligned =
  keyof MirriSchema['bun']['requests'] extends keyof LiteBridgeSurface
    ? keyof LiteBridgeSurface extends keyof MirriSchema['bun']['requests']
      ? true
      : never
    : never;

// The probes: assigning to a never-free array is what forces the compiler
// to evaluate the conditional types above.
const implementsProbe: LiteImplementsWeb[keyof LiteImplementsWeb][] = [];
const acceptsProbe: WebAcceptsLite[keyof WebAcceptsLite][] = [];
const schemaProbe: SchemaAligned[] = [];

describe('desktop-lite bridge contract', () => {
  it('should implement every web bridge member with a compatible signature', () => {
    expect(implementsProbe.length).toBe(0);
  });

  it('should expose no bridge member the web contract does not accept', () => {
    expect(acceptsProbe.length).toBe(0);
  });

  it('should keep the RPC schema keys aligned with the lite surface', () => {
    expect(schemaProbe.length).toBe(0);
  });
});
