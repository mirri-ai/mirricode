// apps/mirri-web/src/api/index.ts
// Singleton factory for the MirriWebApi daemon client.

import { readMirriApiConfig } from './config';
import type { MirriWebApi } from './types';
import { DaemonMirriWebApi } from './daemon/client';

let singleton: MirriWebApi | undefined;

export function getMirriWebApi(): MirriWebApi {
  singleton ??= new DaemonMirriWebApi(readMirriApiConfig());
  return singleton;
}
