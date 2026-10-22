// apps/mirri-desktop/src/main/bridge-contract.test.ts
//
// The shell-bridge contract gate. The web package owns the contract
// (apps/mirri-web/src/lib/desktopBridgeContract.ts — the consumer's
// structural runtime check enforces it at runtime); this file enforces it
// at compile time from the IMPLEMENTOR side: `DesktopBridgeApi` (what the
// preload actually exposes) must be bidirectionally assignable to
// `MirriDesktopBridge` (what the web UI calls). If the two definitions
// drift — a method renamed, a signature changed, a member added on either
// side — the type assertions below fail `tsc --noEmit` and the desktop
// suite here. Import is type-only: zero runtime coupling, erased at emit,
// nothing lands in out/main.cjs or out/preload.cjs.

import { describe, expect, it } from 'vitest';

import type { MirriDesktopBridge } from '../../../mirri-web/src/lib/desktopBridgeContract';

import type { DesktopBridgeApi } from './ipc';

/**
 * Desktop implements the web contract exactly: every `MirriDesktopBridge`
 * member is provided by `DesktopBridgeApi` with a compatible signature.
 * Fails to compile when the web contract grows a method the shell does not
 * implement (the stale-bundle case, caught before release instead of at
 * runtime degradation).
 */
type DesktopImplementsWeb = {
  [K in keyof MirriDesktopBridge]: DesktopBridgeApi[K] extends MirriDesktopBridge[K]
    ? true
    : never;
};

/**
 * Web accepts the desktop surface exactly: every `DesktopBridgeApi` member
 * exists on `MirriDesktopBridge` with a compatible signature. Fails when the
 * shell exposes something (or a wider signature) the web contract does not
 * know — the half-working-bridge case.
 */
type WebAcceptsDesktop = {
  [K in keyof DesktopBridgeApi]: MirriDesktopBridge[K] extends DesktopBridgeApi[K]
    ? true
    : never;
};

// The mapped-type probes: assigning them to a `never`-free record is what
// actually forces the compiler to evaluate the conditional types above.
const implementsProbe: DesktopImplementsWeb[keyof DesktopImplementsWeb][] = [];
const acceptsProbe: WebAcceptsDesktop[keyof WebAcceptsDesktop][] = [];

describe('desktop bridge contract', () => {
  it('should implement every web bridge member with a compatible signature', () => {
    expect(implementsProbe.length).toBe(0);
  });

  it('should expose no bridge member the web contract does not accept', () => {
    expect(acceptsProbe.length).toBe(0);
  });
});
