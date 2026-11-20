-- A person's GitHub account, proven by signing in with GitHub. Only the
-- numeric id is used to match pushes (a login can be renamed); the login is
-- kept to show them which account is connected. No GitHub access token is stored.
alter table users
    add column github_id bigint unique,
    add column github_login text;

-- Pushes GitHub told us about, from connected people only. Times come from
-- GitHub's own signed notification, never from commit dates, which anyone can
-- set to anything. The body hash is the key: GitHub's delivery id header is
-- not covered by its signature, so a replayed body could carry a fresh one.
create table pushes (
    body_sha256 text primary key,
    delivery_id text not null,
    user_id bigint not null references users (id) on delete cascade,
    -- lowercase owner/name, so it compares with a project key lowercased
    repo text not null,
    ref text not null,
    head_sha text not null,
    commit_shas text[] not null default '{}',
    forced boolean not null,
    pushed_at bigint,
    received_at bigint not null
);
create index pushes_user_repo on pushes (user_id, repo);
