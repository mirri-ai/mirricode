/**
 * Bun 运行时预留 adapter。
 *
 * 未来切 Bun 时在此实现 `createBunSqliteDriver(path: string): SqliteDriver`：
 * 包装 `bun:sqlite` 的 `Database`（`exec` / `query(...).get|all|run` / `close`），
 * 并在 `#/persistence/sqlite/factory` 的 `case 'bun'` 分支返回它。
 *
 * 注意：Node 构建下直接 `import('bun:sqlite')` 会解析失败——adapter 需由
 * factory 按需动态加载或按运行时拆产物，见 design.md §5.3 的约束说明。
 *
 * 契约归一化：`bun:sqlite` 的 `Statement.get()` 在无匹配行时返回 `null`，而
 * `SqlStatement.get(): unknown | undefined` 契约要求返回 `undefined`（Node 实现
 * 返回 `undefined`）——未来 Bun adapter 必须把 `null` 归一化为 `undefined`。
 * 此外 `run` 的结果结构、以及 `Uint8Array` / `bigint` 的绑定行为在不同引擎间
 * 可能不同，实现时必须自行验证。
 */
