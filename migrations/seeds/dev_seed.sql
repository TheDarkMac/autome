-- F1.5.g — Seed de données de DÉVELOPPEMENT.
-- N'est PAS une migration : à appliquer explicitement sur une base de dev
-- (jamais en prod). IDs figés pour des tests reproductibles.
-- Schéma aligné sur le domaine (IDs logiques TEXT, *_ms) — cf. migration 0009.
-- Le workflow et le run de démo appartiennent au compte de dev : ids de
-- workflow namespacés par propriétaire (migration 0014, #188).
-- Le graph respecte le schéma F1.4 (autome-core::WorkflowGraph).

BEGIN;

INSERT INTO users (id, email, password_hash, role)
VALUES (
    '00000000-0000-0000-0000-000000000001',
    'dev@autome.local',
    -- Argon2id (PHC) du mot de passe de DEV `autome-dev-password` — connexion
    -- via `POST /auth/login`. Ne jamais réutiliser hors base de dev.
    '$argon2id$v=19$m=19456,t=2,p=1$tgpscoPhcSuThQd6egmrLw$FknLiARlQHDBxMAu8p9aZ9RDLotN21vSC36+JcBpDig',
    'admin'
);

-- Super-admin de dev : wildcard RBAC (`user_permissions.user_id` = claims.sub).
INSERT INTO user_permissions (user_id, permission)
VALUES ('00000000-0000-0000-0000-000000000001', '*');

INSERT INTO workflows (owner_id, id, name, active, current_version, created_at_ms, updated_at_ms)
VALUES ('00000000-0000-0000-0000-000000000001', 'wf-demo', 'Demo : Webhook -> HTTP', TRUE, 1,
        1700000000000, 1700000000000);

INSERT INTO workflow_versions (owner_id, workflow_id, version, graph)
VALUES (
    '00000000-0000-0000-0000-000000000001',
    'wf-demo',
    1,
    '{
      "id": "wf-demo",
      "name": "Demo : Webhook -> HTTP",
      "nodes": [
        { "id": "trigger", "name": "Webhook", "kind": "trigger",
          "trigger_type": "webhook", "params": { "path": "/hook" } },
        { "id": "act", "name": "HTTP", "kind": "action",
          "action_type": "http_request",
          "params": { "method": "POST", "url": "https://example.com" } }
      ],
      "edges": [ { "from": "trigger", "to": "act" } ]
    }'::jsonb
);

INSERT INTO runs (id, workflow_id, version, status, trigger_type,
                  started_at_ms, finished_at_ms, created_at_ms, owner_id)
VALUES ('run-demo', 'wf-demo', 1, 'succeeded', 'webhook',
        1700000000000, 1700000060000, 1700000000000,
        '00000000-0000-0000-0000-000000000001');

INSERT INTO run_steps (run_id, node_id, output)
VALUES
    ('run-demo', 'trigger', '{"body": {"hello": "world"}}'::jsonb),
    ('run-demo', 'act', '{"status": 200}'::jsonb);

INSERT INTO credentials (owner_id, provider, name, secret_enc)
VALUES (
    '00000000-0000-0000-0000-000000000001',
    'slack',
    'workspace-dev',
    -- Placeholder chiffré (le chiffrement réel AES-GCM/KMS = F6.3).
    '\x00'::bytea
);

COMMIT;
