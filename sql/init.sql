-- Canonical QBot schema for a fresh PostgreSQL database.
-- Existing installations are migrated manually before deploying matching code.

CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS pgcrypto;

SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: account_link_challenge; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE account_link_challenge (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    group_id bigint NOT NULL,
    token_hash character varying(64) NOT NULL,
    initiator_account_id uuid NOT NULL,
    target_account_id uuid NOT NULL,
    initiator_entity_id uuid NOT NULL,
    target_entity_id uuid NOT NULL,
    initiator_entity_revision bigint NOT NULL,
    target_entity_revision bigint NOT NULL,
    status character varying(32) DEFAULT 'pending'::character varying NOT NULL,
    created_event_id uuid NOT NULL,
    confirmed_event_id uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    confirmed_at timestamp with time zone,
    cancelled_at timestamp with time zone,
    CONSTRAINT account_link_distinct_accounts CHECK ((initiator_account_id <> target_account_id)),
    CONSTRAINT account_link_revision_valid CHECK (((initiator_entity_revision >= 1) AND (target_entity_revision >= 1))),
    CONSTRAINT account_link_status_valid CHECK (((status)::text = ANY ((ARRAY['pending'::character varying, 'applied'::character varying, 'cancelled'::character varying, 'expired'::character varying])::text[])))
);


--
-- Name: alias; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE alias (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    alias_text character varying(256) NOT NULL,
    normalized_text character varying(256) NOT NULL,
    target_entity_id uuid,
    group_id bigint,
    alias_type character varying(32),
    confidence real NOT NULL,
    status character varying(32) DEFAULT 'candidate'::character varying NOT NULL,
    valid_from timestamp with time zone,
    valid_to timestamp with time zone,
    last_used_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    target_account_id uuid,
    CONSTRAINT alias_exactly_one_target CHECK ((num_nonnulls(target_entity_id, target_account_id) = 1))
);


--
-- Name: alias_evidence; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE alias_evidence (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    alias_id uuid NOT NULL,
    raw_event_id uuid,
    evidence_type character varying(64) NOT NULL,
    evidence_score real,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: cost_ledger; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE cost_ledger (
    day date NOT NULL,
    group_id bigint DEFAULT 0 NOT NULL,
    kind character varying(32) NOT NULL,
    model character varying(128) NOT NULL,
    user_id character varying(64) DEFAULT ''::character varying NOT NULL,
    calls bigint DEFAULT 0 NOT NULL,
    in_hit bigint DEFAULT 0 NOT NULL,
    in_miss bigint DEFAULT 0 NOT NULL,
    "out" bigint DEFAULT 0 NOT NULL,
    cny numeric(12,6) DEFAULT 0 NOT NULL
);


--
-- Name: embedding_index; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE embedding_index (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    group_id bigint,
    object_type character varying(32) NOT NULL,
    object_id uuid NOT NULL,
    embedding public.vector(2048) NOT NULL,
    embedding_model character varying(128) NOT NULL,
    embedding_version integer DEFAULT 1 NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: entity; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE entity (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    entity_type character varying(32) NOT NULL,
    canonical_name character varying(256),
    status character varying(32) DEFAULT 'active'::character varying NOT NULL,
    merged_into uuid,
    revision bigint DEFAULT 1 NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: episode; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE episode (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    group_id bigint NOT NULL,
    episode_type character varying(64),
    title character varying(256),
    summary text NOT NULL,
    started_at timestamp with time zone,
    ended_at timestamp with time zone,
    importance real,
    confidence real,
    status character varying(32) DEFAULT 'active'::character varying NOT NULL,
    revision bigint DEFAULT 1 NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    extraction_id uuid
);


--
-- Name: episode_event; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE episode_event (
    episode_id uuid NOT NULL,
    raw_event_id uuid NOT NULL
);


--
-- Name: group_blocklist; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE group_blocklist (
    group_id bigint NOT NULL,
    user_id character varying(128),
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    blocked_until timestamp with time zone,
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    entity_id uuid,
    CONSTRAINT group_blocklist_exactly_one_target CHECK ((num_nonnulls(user_id, entity_id) = 1))
);


--
-- Name: group_state; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE group_state (
    group_id bigint NOT NULL,
    muted boolean DEFAULT false NOT NULL,
    first_seen_at timestamp with time zone,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: identity_account; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE identity_account (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    entity_id uuid NOT NULL,
    platform character varying(32) NOT NULL,
    platform_user_id character varying(128) NOT NULL,
    first_seen_at timestamp with time zone,
    last_seen_at timestamp with time zone
);


--
-- Name: image_cache; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE image_cache (
    key character varying(64) NOT NULL,
    description text DEFAULT ''::text NOT NULL,
    file_id character varying(64),
    file_provider character varying(32),
    file_uploaded_at timestamp with time zone,
    described_at timestamp with time zone,
    refused boolean DEFAULT false NOT NULL,
    hit_count bigint DEFAULT 0 NOT NULL,
    last_seen timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT image_cache_described_stamped CHECK (((description = ''::text) OR (described_at IS NOT NULL)))
);


--
-- Name: memory_candidate; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE memory_candidate (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    group_id bigint,
    source_event_id uuid,
    candidate_type character varying(64) NOT NULL,
    payload jsonb NOT NULL,
    confidence real,
    status character varying(32) DEFAULT 'pending'::character varying NOT NULL,
    reject_reason text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    processed_at timestamp with time zone,
    extraction_id uuid NOT NULL
);


--
-- Name: memory_extraction; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE memory_extraction (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    group_id bigint NOT NULL,
    status character varying(32) NOT NULL,
    snapshot jsonb,
    candidate_count integer DEFAULT 0 NOT NULL,
    started_at timestamp with time zone DEFAULT now() NOT NULL,
    staged_at timestamp with time zone,
    applied_at timestamp with time zone,
    CONSTRAINT memory_extraction_live_snapshot_v2 CHECK ((((status)::text = 'applied'::text) OR (((status)::text = 'extracting'::text) AND (snapshot IS NULL)) OR (((status)::text = 'staged'::text) AND (snapshot IS NOT NULL) AND (snapshot @> '{"version": 2}'::jsonb)))),
    CONSTRAINT memory_extraction_status_valid CHECK (((status)::text = ANY ((ARRAY['extracting'::character varying, 'staged'::character varying, 'applied'::character varying])::text[])))
);


--
-- Name: memory_extraction_event; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE memory_extraction_event (
    extraction_id uuid NOT NULL,
    raw_event_id uuid NOT NULL,
    ordinal integer NOT NULL
);


--
-- Name: memory_fact; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE memory_fact (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    group_id bigint,
    subject_entity_id uuid,
    predicate character varying(128) NOT NULL,
    object_key text,
    object_entity_id uuid,
    object_value jsonb,
    memory_type character varying(64) NOT NULL,
    confidence real NOT NULL,
    status character varying(32) DEFAULT 'active'::character varying NOT NULL,
    valid_from timestamp with time zone,
    valid_to timestamp with time zone,
    first_observed_at timestamp with time zone,
    last_confirmed_at timestamp with time zone,
    revision bigint DEFAULT 1 NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    subject_account_id uuid,
    CONSTRAINT fact_exactly_one_subject CHECK ((num_nonnulls(subject_entity_id, subject_account_id) = 1))
);


--
-- Name: memory_fact_evidence; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE memory_fact_evidence (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    fact_id uuid NOT NULL,
    raw_event_id uuid NOT NULL,
    relation character varying(32) NOT NULL,
    evidence_score real,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: memory_job; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE memory_job (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    job_type character varying(64) NOT NULL,
    payload jsonb NOT NULL,
    status character varying(32) DEFAULT 'pending'::character varying NOT NULL,
    priority integer DEFAULT 0 NOT NULL,
    retry_count integer DEFAULT 0 NOT NULL,
    max_retry integer DEFAULT 5 NOT NULL,
    available_at timestamp with time zone DEFAULT now() NOT NULL,
    locked_at timestamp with time zone,
    locked_by character varying(128),
    last_error text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    finished_at timestamp with time zone
);


--
-- Name: raw_event; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE raw_event (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    platform character varying(32) NOT NULL,
    event_type character varying(64) NOT NULL,
    group_id bigint,
    platform_user_id character varying(128),
    platform_event_id character varying(128),
    occurred_at timestamp with time zone NOT NULL,
    payload jsonb NOT NULL,
    plain_text text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    archive_schema smallint DEFAULT 1 NOT NULL,
    CONSTRAINT raw_event_archive_schema_valid CHECK ((archive_schema >= 1))
);


--
-- Name: reply_trace; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE reply_trace (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    group_id bigint NOT NULL,
    reply_event_id character varying(64) NOT NULL,
    memo jsonb NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: user_agreement; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE user_agreement (
    group_id bigint NOT NULL,
    user_id character varying(128) NOT NULL,
    version integer DEFAULT 1 NOT NULL,
    agreed_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: account_link_challenge account_link_challenge_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY account_link_challenge
    ADD CONSTRAINT account_link_challenge_pkey PRIMARY KEY (id);


--
-- Name: account_link_challenge account_link_challenge_token_hash_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY account_link_challenge
    ADD CONSTRAINT account_link_challenge_token_hash_key UNIQUE (token_hash);


--
-- Name: alias_evidence alias_evidence_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY alias_evidence
    ADD CONSTRAINT alias_evidence_pkey PRIMARY KEY (id);


--
-- Name: alias alias_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY alias
    ADD CONSTRAINT alias_pkey PRIMARY KEY (id);


--
-- Name: cost_ledger cost_ledger_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY cost_ledger
    ADD CONSTRAINT cost_ledger_pkey PRIMARY KEY (day, group_id, kind, model, user_id);


--
-- Name: embedding_index embedding_index_object_type_object_id_embedding_model_embed_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY embedding_index
    ADD CONSTRAINT embedding_index_object_type_object_id_embedding_model_embed_key UNIQUE (object_type, object_id, embedding_model, embedding_version);


--
-- Name: embedding_index embedding_index_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY embedding_index
    ADD CONSTRAINT embedding_index_pkey PRIMARY KEY (id);


--
-- Name: entity entity_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY entity
    ADD CONSTRAINT entity_pkey PRIMARY KEY (id);


--
-- Name: episode_event episode_event_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY episode_event
    ADD CONSTRAINT episode_event_pkey PRIMARY KEY (episode_id, raw_event_id);


--
-- Name: episode episode_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY episode
    ADD CONSTRAINT episode_pkey PRIMARY KEY (id);


--
-- Name: group_blocklist group_blocklist_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY group_blocklist
    ADD CONSTRAINT group_blocklist_pkey PRIMARY KEY (id);


--
-- Name: group_state group_state_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY group_state
    ADD CONSTRAINT group_state_pkey PRIMARY KEY (group_id);


--
-- Name: identity_account identity_account_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY identity_account
    ADD CONSTRAINT identity_account_pkey PRIMARY KEY (id);


--
-- Name: identity_account identity_account_platform_platform_user_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY identity_account
    ADD CONSTRAINT identity_account_platform_platform_user_id_key UNIQUE (platform, platform_user_id);


--
-- Name: image_cache image_cache_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY image_cache
    ADD CONSTRAINT image_cache_pkey PRIMARY KEY (key);


--
-- Name: memory_candidate memory_candidate_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY memory_candidate
    ADD CONSTRAINT memory_candidate_pkey PRIMARY KEY (id);


--
-- Name: memory_extraction_event memory_extraction_event_extraction_id_ordinal_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY memory_extraction_event
    ADD CONSTRAINT memory_extraction_event_extraction_id_ordinal_key UNIQUE (extraction_id, ordinal);


--
-- Name: memory_extraction_event memory_extraction_event_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY memory_extraction_event
    ADD CONSTRAINT memory_extraction_event_pkey PRIMARY KEY (extraction_id, raw_event_id);


--
-- Name: memory_extraction_event memory_extraction_event_raw_event_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY memory_extraction_event
    ADD CONSTRAINT memory_extraction_event_raw_event_id_key UNIQUE (raw_event_id);


--
-- Name: memory_extraction memory_extraction_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY memory_extraction
    ADD CONSTRAINT memory_extraction_pkey PRIMARY KEY (id);


--
-- Name: memory_fact_evidence memory_fact_evidence_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY memory_fact_evidence
    ADD CONSTRAINT memory_fact_evidence_pkey PRIMARY KEY (id);


--
-- Name: memory_fact memory_fact_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY memory_fact
    ADD CONSTRAINT memory_fact_pkey PRIMARY KEY (id);


--
-- Name: memory_job memory_job_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY memory_job
    ADD CONSTRAINT memory_job_pkey PRIMARY KEY (id);


--
-- Name: raw_event raw_event_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY raw_event
    ADD CONSTRAINT raw_event_pkey PRIMARY KEY (id);


--
-- Name: reply_trace reply_trace_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY reply_trace
    ADD CONSTRAINT reply_trace_pkey PRIMARY KEY (id);


--
-- Name: user_agreement user_agreement_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY user_agreement
    ADD CONSTRAINT user_agreement_pkey PRIMARY KEY (group_id, user_id);


--
-- Name: account_link_pending_initiator; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX account_link_pending_initiator ON account_link_challenge USING btree (initiator_account_id, expires_at) WHERE ((status)::text = 'pending'::text);


--
-- Name: account_link_pending_pair; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX account_link_pending_pair ON account_link_challenge USING btree (group_id, LEAST(initiator_account_id, target_account_id), GREATEST(initiator_account_id, target_account_id)) WHERE ((status)::text = 'pending'::text);


--
-- Name: account_link_pending_target; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX account_link_pending_target ON account_link_challenge USING btree (target_account_id, expires_at) WHERE ((status)::text = 'pending'::text);


--
-- Name: alias_by_account; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX alias_by_account ON alias USING btree (target_account_id) WHERE (target_account_id IS NOT NULL);


--
-- Name: alias_by_entity; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX alias_by_entity ON alias USING btree (target_entity_id) WHERE (target_entity_id IS NOT NULL);


--
-- Name: alias_evidence_probe; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX alias_evidence_probe ON alias_evidence USING btree (alias_id, evidence_type, created_at);


--
-- Name: alias_lookup; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX alias_lookup ON alias USING btree (normalized_text) WHERE ((status)::text <> 'inactive'::text);


--
-- Name: alias_unique_account_scope; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX alias_unique_account_scope ON alias USING btree (COALESCE(group_id, (0)::bigint), normalized_text, target_account_id) WHERE (target_account_id IS NOT NULL);


--
-- Name: alias_unique_entity_scope; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX alias_unique_entity_scope ON alias USING btree (COALESCE(group_id, (0)::bigint), normalized_text, target_entity_id) WHERE (target_entity_id IS NOT NULL);


--
-- Name: candidate_pending; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX candidate_pending ON memory_candidate USING btree (group_id, created_at) WHERE ((status)::text = 'pending'::text);


--
-- Name: embedding_group; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX embedding_group ON embedding_index USING btree (group_id, object_type);


--
-- Name: entity_alive; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX entity_alive ON entity USING btree (entity_type) WHERE ((status)::text = 'active'::text);


--
-- Name: episode_extraction; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX episode_extraction ON episode USING btree (extraction_id) WHERE (extraction_id IS NOT NULL);


--
-- Name: episode_group_time; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX episode_group_time ON episode USING btree (group_id, started_at DESC);


--
-- Name: fact_account_subject; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX fact_account_subject ON memory_fact USING btree (group_id, subject_account_id, status) WHERE (subject_account_id IS NOT NULL);


--
-- Name: fact_entity_subject; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX fact_entity_subject ON memory_fact USING btree (group_id, subject_entity_id, status) WHERE (subject_entity_id IS NOT NULL);


--
-- Name: fact_evidence_fact; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX fact_evidence_fact ON memory_fact_evidence USING btree (fact_id);


--
-- Name: fact_one_current_account; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX fact_one_current_account ON memory_fact USING btree (COALESCE(group_id, (0)::bigint), subject_account_id, predicate, COALESCE(object_key, ''::text)) WHERE ((subject_account_id IS NOT NULL) AND (valid_to IS NULL) AND ((status)::text = 'active'::text));


--
-- Name: fact_one_current_entity; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX fact_one_current_entity ON memory_fact USING btree (COALESCE(group_id, (0)::bigint), subject_entity_id, predicate, COALESCE(object_key, ''::text)) WHERE ((subject_entity_id IS NOT NULL) AND (valid_to IS NULL) AND ((status)::text = 'active'::text));


--
-- Name: group_blocklist_account; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX group_blocklist_account ON group_blocklist USING btree (group_id, user_id) WHERE (user_id IS NOT NULL);


--
-- Name: group_blocklist_expiry; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX group_blocklist_expiry ON group_blocklist USING btree (group_id, blocked_until);


--
-- Name: group_blocklist_holder; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX group_blocklist_holder ON group_blocklist USING btree (group_id, entity_id) WHERE (entity_id IS NOT NULL);


--
-- Name: identity_account_entity; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX identity_account_entity ON identity_account USING btree (entity_id);


--
-- Name: job_claimable; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX job_claimable ON memory_job USING btree (priority DESC, available_at) WHERE ((status)::text = 'pending'::text);


--
-- Name: job_pending_once; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX job_pending_once ON memory_job USING btree (job_type, ((payload ->> 'group_id'::text))) WHERE ((status)::text = 'pending'::text);


--
-- Name: memory_extraction_group_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX memory_extraction_group_status ON memory_extraction USING btree (group_id, status, started_at);


--
-- Name: raw_event_group_created; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX raw_event_group_created ON raw_event USING btree (group_id, created_at, id) WHERE ((event_type)::text = ANY ((ARRAY['message'::character varying, 'notice'::character varying])::text[]));


--
-- Name: raw_event_group_time; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX raw_event_group_time ON raw_event USING btree (group_id, occurred_at DESC);


--
-- Name: raw_event_platform_key; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX raw_event_platform_key ON raw_event USING btree (platform, platform_event_id) WHERE (platform_event_id IS NOT NULL);


--
-- Name: raw_event_speaker; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX raw_event_speaker ON raw_event USING btree (group_id, platform_user_id, occurred_at DESC);


--
-- Name: reply_trace_expiry; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX reply_trace_expiry ON reply_trace USING btree (expires_at) WHERE (expires_at IS NOT NULL);


--
-- Name: reply_trace_reply; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX reply_trace_reply ON reply_trace USING btree (group_id, reply_event_id);


--
-- Name: account_link_challenge account_link_challenge_confirmed_event_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY account_link_challenge
    ADD CONSTRAINT account_link_challenge_confirmed_event_id_fkey FOREIGN KEY (confirmed_event_id) REFERENCES raw_event(id);


--
-- Name: account_link_challenge account_link_challenge_created_event_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY account_link_challenge
    ADD CONSTRAINT account_link_challenge_created_event_id_fkey FOREIGN KEY (created_event_id) REFERENCES raw_event(id);


--
-- Name: account_link_challenge account_link_challenge_initiator_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY account_link_challenge
    ADD CONSTRAINT account_link_challenge_initiator_account_id_fkey FOREIGN KEY (initiator_account_id) REFERENCES identity_account(id);


--
-- Name: account_link_challenge account_link_challenge_initiator_entity_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY account_link_challenge
    ADD CONSTRAINT account_link_challenge_initiator_entity_id_fkey FOREIGN KEY (initiator_entity_id) REFERENCES entity(id);


--
-- Name: account_link_challenge account_link_challenge_target_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY account_link_challenge
    ADD CONSTRAINT account_link_challenge_target_account_id_fkey FOREIGN KEY (target_account_id) REFERENCES identity_account(id);


--
-- Name: account_link_challenge account_link_challenge_target_entity_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY account_link_challenge
    ADD CONSTRAINT account_link_challenge_target_entity_id_fkey FOREIGN KEY (target_entity_id) REFERENCES entity(id);


--
-- Name: alias_evidence alias_evidence_alias_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY alias_evidence
    ADD CONSTRAINT alias_evidence_alias_id_fkey FOREIGN KEY (alias_id) REFERENCES alias(id) ON DELETE CASCADE;


--
-- Name: alias_evidence alias_evidence_raw_event_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY alias_evidence
    ADD CONSTRAINT alias_evidence_raw_event_id_fkey FOREIGN KEY (raw_event_id) REFERENCES raw_event(id);


--
-- Name: alias alias_target_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY alias
    ADD CONSTRAINT alias_target_account_id_fkey FOREIGN KEY (target_account_id) REFERENCES identity_account(id);


--
-- Name: alias alias_target_entity_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY alias
    ADD CONSTRAINT alias_target_entity_id_fkey FOREIGN KEY (target_entity_id) REFERENCES entity(id);


--
-- Name: entity entity_merged_into_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY entity
    ADD CONSTRAINT entity_merged_into_fkey FOREIGN KEY (merged_into) REFERENCES entity(id);


--
-- Name: episode_event episode_event_episode_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY episode_event
    ADD CONSTRAINT episode_event_episode_id_fkey FOREIGN KEY (episode_id) REFERENCES episode(id) ON DELETE CASCADE;


--
-- Name: episode_event episode_event_raw_event_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY episode_event
    ADD CONSTRAINT episode_event_raw_event_id_fkey FOREIGN KEY (raw_event_id) REFERENCES raw_event(id);


--
-- Name: episode episode_extraction_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY episode
    ADD CONSTRAINT episode_extraction_id_fkey FOREIGN KEY (extraction_id) REFERENCES memory_extraction(id);


--
-- Name: group_blocklist group_blocklist_entity_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY group_blocklist
    ADD CONSTRAINT group_blocklist_entity_id_fkey FOREIGN KEY (entity_id) REFERENCES entity(id);


--
-- Name: identity_account identity_account_entity_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY identity_account
    ADD CONSTRAINT identity_account_entity_id_fkey FOREIGN KEY (entity_id) REFERENCES entity(id);


--
-- Name: memory_candidate memory_candidate_extraction_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY memory_candidate
    ADD CONSTRAINT memory_candidate_extraction_id_fkey FOREIGN KEY (extraction_id) REFERENCES memory_extraction(id);


--
-- Name: memory_candidate memory_candidate_source_event_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY memory_candidate
    ADD CONSTRAINT memory_candidate_source_event_id_fkey FOREIGN KEY (source_event_id) REFERENCES raw_event(id);


--
-- Name: memory_extraction_event memory_extraction_event_extraction_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY memory_extraction_event
    ADD CONSTRAINT memory_extraction_event_extraction_id_fkey FOREIGN KEY (extraction_id) REFERENCES memory_extraction(id) ON DELETE CASCADE;


--
-- Name: memory_extraction_event memory_extraction_event_raw_event_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY memory_extraction_event
    ADD CONSTRAINT memory_extraction_event_raw_event_id_fkey FOREIGN KEY (raw_event_id) REFERENCES raw_event(id);


--
-- Name: memory_fact_evidence memory_fact_evidence_fact_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY memory_fact_evidence
    ADD CONSTRAINT memory_fact_evidence_fact_id_fkey FOREIGN KEY (fact_id) REFERENCES memory_fact(id) ON DELETE CASCADE;


--
-- Name: memory_fact_evidence memory_fact_evidence_raw_event_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY memory_fact_evidence
    ADD CONSTRAINT memory_fact_evidence_raw_event_id_fkey FOREIGN KEY (raw_event_id) REFERENCES raw_event(id);


--
-- Name: memory_fact memory_fact_object_entity_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY memory_fact
    ADD CONSTRAINT memory_fact_object_entity_id_fkey FOREIGN KEY (object_entity_id) REFERENCES entity(id);


--
-- Name: memory_fact memory_fact_subject_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY memory_fact
    ADD CONSTRAINT memory_fact_subject_account_id_fkey FOREIGN KEY (subject_account_id) REFERENCES identity_account(id);


--
-- Name: memory_fact memory_fact_subject_entity_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY memory_fact
    ADD CONSTRAINT memory_fact_subject_entity_id_fkey FOREIGN KEY (subject_entity_id) REFERENCES entity(id);


--
-- PostgreSQL database dump complete
--
