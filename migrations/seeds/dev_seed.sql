-- F1.5.g — Seed de données de DÉVELOPPEMENT.
-- N'est PAS une migration : à appliquer explicitement sur une base de dev
-- (jamais en prod). UUID figés pour des tests reproductibles.
-- Le graph respecte le schéma F1.4 (autome-core::WorkflowGraph).

BEGIN;
-- La FK workflows.current_version → workflow_versions est différable.
SET CONSTRAINTS ALL DEFERRED;

INSERT INTO users (id, email, password_hash, role)
VALUES (
    '00000000-0000-0000-0000-000000000001',
    'dev@autome.local',
    -- Placeholder : le hachage argon2 réel est posé en F6.1.
    'argon2-placeholder',
    'admin'
);

INSERT INTO workflows (id, owner_id, name, active, current_version)
VALUES (
    '00000000-0000-0000-0000-0000000000a1',
    '00000000-0000-0000-0000-000000000001',
    'Demo : Webhook -> HTTP',
    TRUE,
    1
);

INSERT INTO workflow_versions (id, workflow_id, version, graph)
VALUES (
    '00000000-0000-0000-0000-0000000000b1',
    '00000000-0000-0000-0000-0000000000a1',
    1,
    '{
      "id": "00000000-0000-0000-0000-0000000000a1",
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
                  started_at, finished_at)
VALUES (
    '00000000-0000-0000-0000-0000000000c1',
    '00000000-0000-0000-0000-0000000000a1',
    1,
    'succeeded',
    'webhook',
    now() - INTERVAL '5 minutes',
    now() - INTERVAL '4 minutes'
);

INSERT INTO run_steps (run_id, node_id, status, input, output)
VALUES
    ('00000000-0000-0000-0000-0000000000c1', 'trigger', 'succeeded',
     '{}'::jsonb, '{"body": {"hello": "world"}}'::jsonb),
    ('00000000-0000-0000-0000-0000000000c1', 'act', 'succeeded',
     '{"body": {"hello": "world"}}'::jsonb, '{"status": 200}'::jsonb);

INSERT INTO credentials (owner_id, provider, name, secret_enc)
VALUES (
    '00000000-0000-0000-0000-000000000001',
    'slack',
    'workspace-dev',
    -- Placeholder chiffré (le chiffrement réel AES-GCM/KMS = F6.3).
    '\x00'::bytea
);

COMMIT;
