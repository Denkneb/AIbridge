# Security fixtures v15 (0A.5)

`fixtures/security-cases.json` — отдельный additive corpus Python v15.
Исторические command/path/Git corpora сохраняются. Эталон — read-only checkout
`/home/denis/Python/agent_bridge`, HEAD
`86c65b55cc7cca0b9e917a36f4f6c317eac4cc1a`.

51 исполняемый case фиксирует девять категорий подозрительных секретов,
категории без значений и позиций, дедупликацию/порядок, отрицательные примеры
и явный `allow_suspected_secrets`; ограничения Git worktree (submodules,
LFS config/attributes, sparse checkout); canonical scope overlap, включая
несуществующий leaf под symlink, broken/loop symlink и corrupt persisted scope.
Все строки с форматом credentials синтетические и не используются для доступа.

Verifier вызывает реальные `_suspected_secret_error`, `check_repo_support`,
`_canonical_scopes_overlap` и `_parse_reserved_scopes`. Git-репозитории и symlinks
создаются только во временных каталогах; runtime SQLite не используется.
Неисполняемые cases считаются ошибками, пропусков нет. Проверяется точный HEAD,
schema version и путь импорта; bytecode в reference checkout не пишется.

```bash
python3 docs/fixtures/security/verify.py
```

Публичный запрет абсолютных `allowed_paths` в worktree и отсутствие side effects
при отказе/повтор с secret override покрыты существующим MCP corpus. Его
verifier вызывает реальные tools на временных БД и с локальными doubles:

```bash
python3 docs/fixtures/mcp/verify.py --target-only
```

Ограничение источника: `check_repo_support` отказывает при обнаруженных
unsupported features, но ошибки Git probes считает отсутствием evidence;
это fidelity существующего Python, а не гарантия отказа при любой Git ошибке.
Corrupt scope и неразрешимые symlink identities отказывают через `ScopeDataError`.
Rust security implementation этим refresh не меняется.
