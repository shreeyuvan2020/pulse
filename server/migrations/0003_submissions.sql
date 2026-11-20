-- The hash chain a computer sends as it records: one row per 5-minute
-- segment, with the time this server received it. This is what makes later
-- uploads checkable: a recording that was not committed here in real time has
-- no row, and a video or timeline that was changed after recording no longer
-- matches its hashes. No FK on token_id: revoking a token must not erase it.
create table chain_links (
    link text primary key,
    user_id bigint not null references users (id) on delete cascade,
    token_id bigint not null,
    previous text not null,
    start_at bigint not null,
    end_at bigint not null,
    video text not null,
    timeline text not null,
    received_at bigint not null,
    check (end_at > start_at and end_at - start_at <= 600)
);
create index chain_links_user_start on chain_links (user_id, start_at);

-- Hours a person submits to a program, with the recordings that back them.
create table submissions (
    id bigserial primary key,
    user_id bigint not null references users (id) on delete cascade,
    project_id bigint not null references projects (id) on delete cascade,
    program text not null,
    status text not null check (status in ('uploading', 'submitted', 'withdrawn')),
    created_at bigint not null,
    submitted_at bigint,
    -- when a program last needs to look at this again (finalized or withdrawn)
    changed_at bigint not null,
    -- frozen when the submission is finalized, in whole seconds
    recorded_seconds bigint,
    active_seconds bigint,
    omitted_seconds bigint,
    claimed_seconds bigint
);
create index submissions_user on submissions (user_id);
create index submissions_program_changed on submissions (program, changed_at);

-- Every segment inside the submitted time range. Segments the person did not
-- submit (other projects, private moments, deleted videos) are listed as
-- omitted so a reviewer sees the gaps instead of a quietly shorter timeline.
create table submission_segments (
    submission_id bigint not null references submissions (id) on delete cascade,
    link text not null references chain_links (link),
    omitted boolean not null,
    video_ok boolean not null default false,
    timeline_ok boolean not null default false,
    primary key (submission_id, link)
);
-- a segment's hours can only be submitted once
create unique index submission_segments_once on submission_segments (link) where not omitted;

-- Every time a program looks at a submission, so its owner can see it.
create table evidence_views (
    id bigserial primary key,
    submission_id bigint not null references submissions (id) on delete cascade,
    program text not null,
    what text not null,
    at bigint not null
);
create index evidence_views_submission on evidence_views (submission_id, at);
