import { createDecorator } from '../../di';
import type { ProfileRegistryEntry } from '../../profile/registry';

export const IProfileService = createDecorator<IProfileService>('profileService');

export interface ProfileEntry {
  name: string;
  description?: string;
  source: ProfileRegistryEntry['source'];
  builtin: boolean;
  essential: boolean;
  enabled: boolean;
  filePath?: string;
  extends?: string;
  defaultModel?: string;
  tools?: readonly string[];
  whenToUse?: string;
}

export interface CreateProfileInput {
  name: string;
  description?: string;
  extends?: string;
  defaultModel?: string;
  tools?: readonly string[];
  systemPromptTemplate?: string;
  whenToUse?: string;
  promptVars?: Record<string, string>;
  capabilities?: readonly string[];
  capabilitiesRequired?: readonly string[];
}

export type UpdateProfileInput = Partial<CreateProfileInput>;

export interface IProfileService {
  listProfiles(): Promise<readonly ProfileEntry[]>;
  getProfile(name: string): Promise<ProfileEntry | undefined>;
  createProfile(data: CreateProfileInput): Promise<ProfileEntry>;
  updateProfile(name: string, data: UpdateProfileInput): Promise<ProfileEntry>;
  deleteProfile(name: string): Promise<void>;
  enableProfile(name: string): Promise<void>;
  disableProfile(name: string): Promise<void>;
}
