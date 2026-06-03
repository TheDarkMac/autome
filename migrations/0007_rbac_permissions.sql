-- Permissions par utilisateur (dette #107, constat 2).
-- Backe `PermissionStore` côté PostgreSQL (cf. autome-persistence) :
-- persistées et partagées entre répliques, contrairement à l'impl in-memory.
CREATE TABLE user_permissions (
    -- Acteur (claims.sub). Référence logique vers users(id) ; on n'impose pas
    -- de FK ici pour rester découplé de la stratégie d'identité.
    user_id    TEXT NOT NULL,
    -- Permission accordée (ex. `workflows.read`).
    permission TEXT NOT NULL,
    PRIMARY KEY (user_id, permission)
);
