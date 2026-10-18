import { mkdir, writeFile } from 'node:fs/promises';
import { join } from 'node:path';

/**
 * Write a minimal `config.toml` into a rust-engine test home so sessions can
 * be created without a real provider: one model alias plus its backing
 * provider entry, and `default_model` pointing at the alias.
 *
 * The rust engine has no builtin model defaults — `session.create` without a
 * configured model fails with "no model configured". These tests never send
 * a prompt, so the provider is never dialed; the dead `baseUrl` (port 1)
 * makes any accidental call fail fast instead of hanging.
 *
 * @param homeDir the harness/engine home directory the config is written to
 * @param alias the model alias to define; `default_model` points at it
 */
export async function writeRustEngineModelConfig(
  homeDir: string,
  alias = 'rust-test-model',
): Promise<void> {
  await mkdir(homeDir, { recursive: true });
  await writeFile(
    join(homeDir, 'config.toml'),
    [
      `default_model = "${alias}"`,
      '',
      `[models.${alias}]`,
      'provider = "rust-test"',
      `model = "${alias}"`,
      'max_context_size = 128000',
      '',
      '[providers.rust-test]',
      'type = "openai"',
      'api_key = "test-key"',
      'base_url = "http://127.0.0.1:1/v1"',
      '',
    ].join('\n'),
    'utf-8',
  );
}
