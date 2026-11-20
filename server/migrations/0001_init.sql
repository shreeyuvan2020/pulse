-- All times are unix seconds (UTC) and all durations are integer seconds.

create table users (
    id bigserial primary key,
    hca_sub text not null unique,
    username text not null unique,
    display_name text not null,
    profile_public boolean not null default true,
    created_at bigint not null default extract(epoch from now())::bigint
);

-- Browser sessions. Only a hash of the cookie value is stored.
create table sessions (
    token_hash text primary key,
    user_id bigint not null references users (id) on delete cascade,
    expires_at bigint not null
);
create index sessions_expires_at on sessions (expires_at);

-- One token per computer running Pulse. Only a hash is stored.
create table api_tokens (
    id bigserial primary key,
    user_id bigint not null references users (id) on delete cascade,
    token_hash text not null unique,
    name text not null,
    created_at bigint not null default extract(epoch from now())::bigint,
    last_used_at bigint
);
create index api_tokens_user on api_tokens (user_id);

create table projects (
    id bigserial primary key,
    user_id bigint not null references users (id) on delete cascade,
    key text not null,
    public boolean not null,
    created_at bigint not null default extract(epoch from now())::bigint,
    unique (user_id, key)
);

-- Credited time: how many seconds of a 5-minute slot a device spent on a
-- project. A client sends the whole slot again as it grows, so rows are replaced.
-- token_id has no foreign key on purpose: revoking a token must not erase the
-- time it already logged.
create table credits (
    user_id bigint not null references users (id) on delete cascade,
    slot bigint not null check (slot % 300 = 0),
    project_id bigint not null references projects (id) on delete cascade,
    token_id bigint not null,
    seconds integer not null check (seconds between 1 and 300),
    prompts integer not null default 0,
    sources text[] not null default '{}',
    models text[] not null default '{}',
    updated_at bigint not null,
    primary key (user_id, slot, project_id, token_id)
);
create index credits_project on credits (project_id);
