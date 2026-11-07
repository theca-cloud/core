--
-- PostgreSQL database dump
--

\restrict 21m7mnhE4BhamCaq0PWXbXdEToIzGVkUwOeOt32sGZqSarVmPD0z8TbV8lxFui6

-- Dumped from database version 18.6 (Debian 18.6-1.pgdg13+2)
-- Dumped by pg_dump version 18.6 (Debian 18.6-1.pgdg13+2)

SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET transaction_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

--
-- Name: pg_trgm; Type: EXTENSION; Schema: -; Owner: -
--

CREATE EXTENSION IF NOT EXISTS pg_trgm WITH SCHEMA public;


--
-- Name: EXTENSION pg_trgm; Type: COMMENT; Schema: -; Owner: -
--

COMMENT ON EXTENSION pg_trgm IS 'text similarity measurement and index searching based on trigrams';


--
-- Name: check_storage_organization(bigint, bigint, text); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.check_storage_organization(storage_pk bigint, org bigint, constraint_name text) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    storage_org bigint;
BEGIN
    SELECT organization INTO storage_org FROM storage WHERE pk = storage_pk;
    IF storage_org IS NOT NULL AND storage_org <> org THEN
        RAISE EXCEPTION 'storage % belongs to another organization', storage_pk
            USING ERRCODE = 'check_violation', CONSTRAINT = constraint_name;
    END IF;
END;
$$;


--
-- Name: chunk_storage_organization(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.chunk_storage_organization() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM check_storage_organization(NEW.storage, NEW.organization, 'chunk_storage_organization');
    RETURN NEW;
END;
$$;


--
-- Name: dedup_scope(bigint, bigint); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.dedup_scope(library bigint, token bigint) RETURNS TABLE(library bigint)
    LANGUAGE sql STABLE
    AS $_$
    SELECT $1
    UNION
    SELECT other.library FROM library_namespace mine
    JOIN library_namespace other ON other.namespace = mine.namespace
    WHERE mine.library = $1
    UNION
    SELECT a.library FROM library_access_any($2) a
    JOIN library l ON l.pk = $1
    WHERE a.organization = l.owner
$_$;


--
-- Name: is_valid_name(text); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.is_valid_name(n text) RETURNS boolean
    LANGUAGE sql IMMUTABLE STRICT
    AS $_$
    SELECT n IS NFC NORMALIZED
       AND n <> ''
       AND octet_length(n) <= 255
       AND n !~ '[<>:"/\\|?*\x01-\x1f\u202a-\u202e\u2066-\u2069]'
       AND n !~ '[ .]$'
       AND upper(split_part(n, '.', 1)) NOT IN (
           'CON', 'PRN', 'AUX', 'NUL',
           'COM1', 'COM2', 'COM3', 'COM4', 'COM5', 'COM6', 'COM7', 'COM8', 'COM9',
           'LPT1', 'LPT2', 'LPT3', 'LPT4', 'LPT5', 'LPT6', 'LPT7', 'LPT8', 'LPT9'
       )
$_$;


--
-- Name: library_access(bigint); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.library_access(token bigint) RETURNS TABLE(library bigint, organization bigint, "user" bigint, can_write boolean)
    LANGUAGE sql STABLE
    AS $_$
    SELECT a.* FROM library_access_any($1) a
    JOIN library l ON l.pk = a.library
    WHERE l.deleted_at IS NULL
$_$;


--
-- Name: library_access_any(bigint); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.library_access_any(token bigint) RETURNS TABLE(library bigint, organization bigint, "user" bigint, can_write boolean)
    LANGUAGE sql STABLE
    AS $_$
    SELECT m.library, o.organization, o."user", o.can_write AND m.role = 'write'
    FROM organization_access($1) o
    JOIN library_member m ON m."user" = o."user"
    WHERE o.library IS NULL OR m.library = o.library
$_$;


--
-- Name: library_storage_organization(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.library_storage_organization() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM check_storage_organization(NEW.storage, NEW.owner, 'library_storage_organization');
    RETURN NEW;
END;
$$;


--
-- Name: node_file_has_version(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.node_file_has_version() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM node WHERE pk = NEW.pk AND is_file AND current_version IS NULL) THEN
        RAISE EXCEPTION 'file % has no current version', NEW.pk
            USING ERRCODE = 'check_violation', CONSTRAINT = 'node_file_has_version';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: node_no_cycle(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.node_no_cycle() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM 1 FROM library WHERE pk = NEW.library FOR NO KEY UPDATE;
    IF EXISTS (
        -- UNION (not UNION ALL): stops even if a cycle somehow existed already.
        WITH RECURSIVE ancestor (pk) AS (
            SELECT NEW.parent
            UNION
            SELECT n.parent FROM node n JOIN ancestor a ON n.pk = a.pk WHERE n.parent IS NOT NULL
        )
        SELECT 1 FROM ancestor WHERE pk = NEW.pk
    ) THEN
        RAISE EXCEPTION 'node % cannot be moved into its own subtree', NEW.pk
            USING ERRCODE = 'check_violation', CONSTRAINT = 'node_no_cycle';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: node_path(bigint); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.node_path(node bigint) RETURNS text
    LANGUAGE sql STABLE
    AS $_$
    WITH RECURSIVE up (parent, name, depth) AS (
        SELECT parent, name, 0 FROM node WHERE pk = $1
        UNION ALL
        SELECT n.parent, n.name, up.depth + 1 FROM node n JOIN up ON n.pk = up.parent
    )
    SELECT '/' || coalesce(string_agg(name, '/' ORDER BY depth DESC) FILTER (WHERE name <> ''), '')
    FROM up
$_$;


--
-- Name: node_reachable(bigint); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.node_reachable(node bigint) RETURNS boolean
    LANGUAGE sql STABLE
    AS $_$
    WITH RECURSIVE up (parent, deleted_at) AS (
        SELECT parent, deleted_at FROM node WHERE pk = $1
        UNION ALL
        SELECT n.parent, n.deleted_at FROM node n JOIN up ON n.pk = up.parent
    )
    SELECT NOT EXISTS (SELECT 1 FROM up WHERE deleted_at IS NOT NULL)
$_$;


--
-- Name: organization_access(bigint); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.organization_access(token bigint) RETURNS TABLE(organization bigint, "user" bigint, library bigint, can_write boolean)
    LANGUAGE sql STABLE
    AS $_$
    SELECT u.organization, u.pk, t.library, NOT t.read_only
    FROM token t
    JOIN account a ON a.uuid = t.resource
    JOIN "user" u ON u.account = a.pk AND u.joined_at IS NOT NULL
    JOIN organization o ON o.pk = u.organization AND o.suspended_at IS NULL
    WHERE t.pk = $1 AND t.usage IN ('session', 'app_password')
      AND (t.organization IS NULL OR u.organization = t.organization)
$_$;


--
-- Name: organization_default_storage(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.organization_default_storage() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.default_storage IS NOT NULL THEN
        PERFORM check_storage_organization(NEW.default_storage, NEW.pk, 'organization_default_storage');
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: storage_organization_immutable(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.storage_organization_immutable() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.organization IS DISTINCT FROM OLD.organization THEN
        RAISE EXCEPTION 'the organization of storage % cannot change', OLD.pk
            USING ERRCODE = 'check_violation', CONSTRAINT = 'storage_organization_immutable';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: user_left_identities(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.user_left_identities() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    DELETE FROM identity i USING identity_provider p
    WHERE i.account = OLD.account AND i.provider = p.pk AND p.organization = OLD.organization;
    RETURN OLD;
END;
$$;


--
-- Name: user_personal_organization(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.user_personal_organization() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM organization WHERE pk = NEW.organization AND personal_of <> NEW.account
    ) THEN
        RAISE EXCEPTION 'organization % is personal', NEW.organization
            USING ERRCODE = 'check_violation', CONSTRAINT = 'personal_organization';
    END IF;
    RETURN NEW;
END;
$$;


SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: _sqlx_migrations; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public._sqlx_migrations (
    version bigint NOT NULL,
    description text NOT NULL,
    installed_on timestamp with time zone DEFAULT now() NOT NULL,
    success boolean NOT NULL,
    checksum bytea NOT NULL,
    execution_time bigint NOT NULL
);


--
-- Name: account; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.account (
    pk bigint NOT NULL,
    uuid uuid DEFAULT uuidv7() NOT NULL,
    email text NOT NULL,
    instance_admin boolean DEFAULT false NOT NULL,
    verified boolean DEFAULT false NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    suspended_at timestamp with time zone
);


--
-- Name: account_pk_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.account ALTER COLUMN pk ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.account_pk_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: chunk; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.chunk (
    pk bigint NOT NULL,
    uuid uuid DEFAULT uuidv7() NOT NULL,
    organization bigint NOT NULL,
    library bigint,
    hash bytea NOT NULL,
    storage bigint NOT NULL,
    plain_size bigint NOT NULL,
    stored_size bigint,
    compression text,
    stored_sha256 bytea,
    declared_by bigint,
    url_expires_at timestamp with time zone,
    uploaded_at timestamp with time zone,
    touched_at timestamp with time zone DEFAULT now() NOT NULL,
    deleting_at timestamp with time zone,
    ready boolean GENERATED ALWAYS AS (((uploaded_at IS NOT NULL) AND (deleting_at IS NULL))) STORED NOT NULL,
    CONSTRAINT chunk_compression_check CHECK ((compression = ANY (ARRAY['none'::text, 'lz4'::text]))),
    CONSTRAINT chunk_declared CHECK ((((stored_size IS NULL) = (compression IS NULL)) AND ((stored_size IS NULL) = (stored_sha256 IS NULL)))),
    CONSTRAINT chunk_hash_check CHECK ((length(hash) = 16)),
    CONSTRAINT chunk_plain_size CHECK (((plain_size >= 0) AND (plain_size <= 8388608))),
    CONSTRAINT chunk_stored_sha256_check CHECK ((length(stored_sha256) = 32)),
    CONSTRAINT chunk_stored_size CHECK (((stored_size >= 0) AND (stored_size <= plain_size))),
    CONSTRAINT chunk_uploaded_declared CHECK (((uploaded_at IS NULL) OR (stored_size IS NOT NULL)))
);


--
-- Name: chunk_in_version; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.chunk_in_version (
    pk bigint NOT NULL,
    uuid uuid DEFAULT uuidv7() NOT NULL,
    organization bigint NOT NULL,
    version bigint NOT NULL,
    chunk bigint NOT NULL,
    chunk_ready boolean DEFAULT true NOT NULL,
    "position" integer NOT NULL,
    CONSTRAINT chunk_in_version_chunk_ready_check CHECK (chunk_ready),
    CONSTRAINT chunk_in_version_position_check CHECK (("position" >= 0))
);


--
-- Name: chunk_in_version_pk_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.chunk_in_version ALTER COLUMN pk ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.chunk_in_version_pk_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: chunk_pk_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.chunk ALTER COLUMN pk ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.chunk_pk_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: email; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.email (
    pk bigint NOT NULL,
    uuid uuid DEFAULT uuidv7() NOT NULL,
    recipient text NOT NULL,
    template text NOT NULL,
    params jsonb NOT NULL,
    attempts integer DEFAULT 0 NOT NULL,
    next_attempt_at timestamp with time zone DEFAULT now() NOT NULL,
    sent_at timestamp with time zone,
    given_up_at timestamp with time zone,
    last_error text,
    CONSTRAINT email_attempts_check CHECK ((attempts >= 0)),
    CONSTRAINT email_done CHECK (((sent_at IS NULL) OR (given_up_at IS NULL)))
);


--
-- Name: email_pk_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.email ALTER COLUMN pk ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.email_pk_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: event; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.event (
    pk bigint NOT NULL,
    library bigint NOT NULL,
    seq bigint NOT NULL,
    resource uuid NOT NULL,
    resource_kind text NOT NULL,
    action text NOT NULL,
    actor bigint,
    at timestamp with time zone DEFAULT now() NOT NULL,
    uuid uuid DEFAULT uuidv7() NOT NULL,
    CONSTRAINT event_action CHECK ((((resource_kind = 'node'::text) AND (action = ANY (ARRAY['created'::text, 'versioned'::text, 'touched'::text, 'version_restored'::text, 'renamed'::text, 'moved'::text, 'trashed'::text, 'restored'::text, 'purged'::text]))) OR ((resource_kind = 'library'::text) AND (action = ANY (ARRAY['renamed'::text, 'trashed'::text, 'restored'::text]))))),
    CONSTRAINT event_seq_positive CHECK ((seq > 0))
);


--
-- Name: event_pk_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.event ALTER COLUMN pk ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.event_pk_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: identity; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.identity (
    pk bigint NOT NULL,
    uuid uuid DEFAULT uuidv7() NOT NULL,
    account bigint NOT NULL,
    kind text NOT NULL,
    provider bigint,
    details jsonb NOT NULL,
    CONSTRAINT identity_kind_check CHECK ((kind = ANY (ARRAY['password'::text, 'oidc'::text, 'totp'::text]))),
    CONSTRAINT identity_oidc_details CHECK (((kind <> 'oidc'::text) OR (NOT (jsonb_typeof((details -> 'subject'::text)) IS DISTINCT FROM 'string'::text)))),
    CONSTRAINT identity_password_details CHECK (((kind <> 'password'::text) OR (NOT (jsonb_typeof((details -> 'hash'::text)) IS DISTINCT FROM 'string'::text)))),
    CONSTRAINT identity_provider_oidc CHECK (((kind = 'oidc'::text) = (provider IS NOT NULL))),
    CONSTRAINT identity_totp_details CHECK (((kind <> 'totp'::text) OR ((NOT (jsonb_typeof((details -> 'secret'::text)) IS DISTINCT FROM 'string'::text)) AND (NOT (jsonb_typeof((details -> 'confirmed'::text)) IS DISTINCT FROM 'boolean'::text)) AND (NOT (jsonb_typeof((details -> 'last_step'::text)) IS DISTINCT FROM 'number'::text)))))
);


--
-- Name: identity_pk_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.identity ALTER COLUMN pk ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.identity_pk_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: identity_provider; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.identity_provider (
    pk bigint NOT NULL,
    uuid uuid DEFAULT uuidv7() NOT NULL,
    organization bigint,
    name text NOT NULL,
    issuer text NOT NULL,
    client_id text NOT NULL,
    client_secret_file text,
    client_secret_encrypted text,
    enabled boolean DEFAULT true NOT NULL,
    CONSTRAINT identity_provider_secret CHECK (((client_secret_file IS NULL) <> (client_secret_encrypted IS NULL))),
    CONSTRAINT identity_provider_secret_file CHECK (((organization IS NULL) OR (client_secret_file IS NULL)))
);


--
-- Name: identity_provider_pk_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.identity_provider ALTER COLUMN pk ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.identity_provider_pk_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: job_run; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.job_run (
    pk bigint NOT NULL,
    uuid uuid DEFAULT uuidv7() NOT NULL,
    job text NOT NULL,
    started_at timestamp with time zone DEFAULT now() NOT NULL,
    finished_at timestamp with time zone,
    status text DEFAULT 'running'::text NOT NULL,
    items integer DEFAULT 0 NOT NULL,
    error text,
    CONSTRAINT job_run_failure_error CHECK (((status <> 'failure'::text) OR (error IS NOT NULL))),
    CONSTRAINT job_run_finished CHECK (((status = 'running'::text) = (finished_at IS NULL))),
    CONSTRAINT job_run_items_check CHECK ((items >= 0)),
    CONSTRAINT job_run_status_check CHECK ((status = ANY (ARRAY['running'::text, 'success'::text, 'failure'::text, 'interrupted'::text])))
);


--
-- Name: job_run_pk_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.job_run ALTER COLUMN pk ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.job_run_pk_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: library; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.library (
    pk bigint NOT NULL,
    uuid uuid DEFAULT uuidv7() NOT NULL,
    name text NOT NULL,
    owner bigint NOT NULL,
    storage bigint NOT NULL,
    deleted_at timestamp with time zone,
    event_seq bigint DEFAULT 0 NOT NULL,
    CONSTRAINT library_name_valid CHECK (public.is_valid_name(name))
);


--
-- Name: library_member; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.library_member (
    pk bigint NOT NULL,
    uuid uuid DEFAULT uuidv7() NOT NULL,
    organization bigint NOT NULL,
    library bigint NOT NULL,
    "user" bigint NOT NULL,
    role text NOT NULL,
    CONSTRAINT library_member_role_check CHECK ((role = ANY (ARRAY['read'::text, 'write'::text])))
);


--
-- Name: library_member_pk_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.library_member ALTER COLUMN pk ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.library_member_pk_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: library_namespace; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.library_namespace (
    pk bigint NOT NULL,
    uuid uuid DEFAULT uuidv7() NOT NULL,
    organization bigint NOT NULL,
    library bigint NOT NULL,
    namespace bigint NOT NULL
);


--
-- Name: library_namespace_pk_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.library_namespace ALTER COLUMN pk ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.library_namespace_pk_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: library_pk_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.library ALTER COLUMN pk ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.library_pk_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: login_flow; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.login_flow (
    pk bigint NOT NULL,
    uuid uuid DEFAULT uuidv7() NOT NULL,
    name text NOT NULL,
    code text NOT NULL,
    poll_hash bytea NOT NULL,
    account bigint,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT login_flow_name_check CHECK (((length(name) >= 1) AND (length(name) <= 200))),
    CONSTRAINT login_flow_poll_hash_check CHECK ((length(poll_hash) = 32))
);


--
-- Name: login_flow_pk_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.login_flow ALTER COLUMN pk ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.login_flow_pk_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: namespace; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.namespace (
    pk bigint NOT NULL,
    uuid uuid DEFAULT uuidv7() NOT NULL,
    organization bigint NOT NULL,
    name text NOT NULL
);


--
-- Name: namespace_pk_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.namespace ALTER COLUMN pk ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.namespace_pk_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: node; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.node (
    pk bigint NOT NULL,
    uuid uuid DEFAULT uuidv7() NOT NULL,
    organization bigint NOT NULL,
    library bigint NOT NULL,
    parent bigint,
    parent_is_file boolean DEFAULT false NOT NULL,
    name text NOT NULL,
    is_file boolean NOT NULL,
    current_version bigint,
    deleted_at timestamp with time zone,
    CONSTRAINT node_folder_has_no_version CHECK ((is_file OR (current_version IS NULL))),
    CONSTRAINT node_name_valid CHECK (((parent IS NULL) OR public.is_valid_name(name))),
    CONSTRAINT node_not_own_parent CHECK (((parent IS NULL) OR (parent <> pk))),
    CONSTRAINT node_parent_is_file_check CHECK ((NOT parent_is_file)),
    CONSTRAINT node_root_is_folder CHECK (((parent IS NOT NULL) OR (NOT is_file))),
    CONSTRAINT node_root_unnamed CHECK (((parent IS NULL) = (name = ''::text)))
);


--
-- Name: node_pk_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.node ALTER COLUMN pk ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.node_pk_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: organization; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.organization (
    pk bigint NOT NULL,
    uuid uuid DEFAULT uuidv7() NOT NULL,
    name text NOT NULL,
    default_storage bigint,
    owner bigint,
    quota bigint,
    owner_joined boolean DEFAULT true NOT NULL,
    personal_of bigint,
    suspended_at timestamp with time zone,
    deleted_at timestamp with time zone,
    CONSTRAINT organization_owner_joined_check CHECK (owner_joined),
    CONSTRAINT organization_owner_present CHECK (((owner IS NOT NULL) OR (deleted_at IS NOT NULL))),
    CONSTRAINT organization_quota CHECK ((quota >= 0))
);


--
-- Name: organization_pk_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.organization ALTER COLUMN pk ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.organization_pk_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: setting; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.setting (
    key text NOT NULL,
    value text NOT NULL
);


--
-- Name: storage; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.storage (
    pk bigint NOT NULL,
    uuid uuid DEFAULT uuidv7() NOT NULL,
    organization bigint,
    name text NOT NULL,
    implementation text NOT NULL,
    details jsonb NOT NULL,
    is_default boolean DEFAULT false NOT NULL
);


--
-- Name: storage_pk_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.storage ALTER COLUMN pk ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.storage_pk_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: token; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.token (
    pk bigint NOT NULL,
    uuid uuid DEFAULT uuidv7() NOT NULL,
    usage text NOT NULL,
    resource uuid NOT NULL,
    prefix text NOT NULL,
    hash bytea NOT NULL,
    user_agent text,
    ip inet,
    location text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    last_seen timestamp with time zone,
    revoked_at timestamp with time zone,
    revocation_reason text,
    name text NOT NULL,
    organization bigint,
    library bigint,
    read_only boolean DEFAULT false NOT NULL,
    CONSTRAINT token_hash_check CHECK ((length(hash) = 32)),
    CONSTRAINT token_library_scope CHECK (((library IS NULL) OR ((usage = 'app_password'::text) AND (organization IS NOT NULL)))),
    CONSTRAINT token_name_check CHECK (((length(name) >= 1) AND (length(name) <= 200))),
    CONSTRAINT token_organization_scope CHECK (((organization IS NULL) OR (usage = ANY (ARRAY['session'::text, 'app_password'::text])))),
    CONSTRAINT token_read_only CHECK (((NOT read_only) OR (usage = 'app_password'::text))),
    CONSTRAINT token_revocation_reason CHECK (((revoked_at IS NULL) = (revocation_reason IS NULL))),
    CONSTRAINT token_usage_check CHECK ((usage = ANY (ARRAY['session'::text, 'email_verification'::text, 'invitation'::text, 'password_reset'::text, 'app_password'::text, 'recovery_code'::text])))
);


--
-- Name: token_pk_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.token ALTER COLUMN pk ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.token_pk_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: user; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public."user" (
    pk bigint NOT NULL,
    uuid uuid DEFAULT uuidv7() NOT NULL,
    account bigint NOT NULL,
    organization bigint NOT NULL,
    joined_at timestamp with time zone,
    invited_at timestamp with time zone,
    joined boolean GENERATED ALWAYS AS ((joined_at IS NOT NULL)) STORED NOT NULL
);


--
-- Name: user_pk_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public."user" ALTER COLUMN pk ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.user_pk_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: version; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.version (
    pk bigint NOT NULL,
    uuid uuid DEFAULT uuidv7() NOT NULL,
    organization bigint NOT NULL,
    file bigint NOT NULL,
    file_is_file boolean DEFAULT true NOT NULL,
    parent_version bigint,
    message text,
    size bigint NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    modified_at timestamp with time zone DEFAULT now() NOT NULL,
    author bigint,
    CONSTRAINT version_file_is_file_check CHECK (file_is_file),
    CONSTRAINT version_size_check CHECK ((size >= 0))
);


--
-- Name: version_pk_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.version ALTER COLUMN pk ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.version_pk_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Data for Name: _sqlx_migrations; Type: TABLE DATA; Schema: public; Owner: -
--

INSERT INTO public._sqlx_migrations VALUES (20261005000000, 'init', '2026-11-07 17:42:16.798361+00', true, '\x15834201ee89c2fa016156514a54e33de8cb625da2509429389964cf0839426b855c73300bfb6aadffa6dc9e638fd3e3', 65660708);
INSERT INTO public._sqlx_migrations VALUES (20261027000000, 'wider dedup', '2026-11-07 17:42:16.868311+00', true, '\x1f539e89dc5bf2c8ed449d0bca2e36a83e0c546c397a7c8fa9c3f26cf776107c6c7669fbc63ffa7ec2b17c4d499f668b', 2660542);
INSERT INTO public._sqlx_migrations VALUES (20261102000000, 'event feed', '2026-11-07 17:42:16.871488+00', true, '\xd53e6fbdb2cd5757b1824420909bea7371d600677d408c25b4e6fa1a32f698a041ea6848fa1e383530a263357d640200', 5335000);
INSERT INTO public._sqlx_migrations VALUES (20261107000000, 'event uuid', '2026-11-07 17:42:16.877432+00', true, '\xedb9c93c67309871352374b6b700dc257c74e108f4c18799b0c6351789eeb4714d82cf54f1c8028de3e32a9ef7273551', 1124959);


--
-- Data for Name: account; Type: TABLE DATA; Schema: public; Owner: -
--

INSERT INTO public.account OVERRIDING SYSTEM VALUE VALUES (1, '01a1b71a-9cda-7f58-bdd0-ca4ab9a00ad5', 'admin@upgrade.theca', true, true, '2026-11-07 17:42:17.048881+00', NULL);
INSERT INTO public.account OVERRIDING SYSTEM VALUE VALUES (2, '01a1b71a-9cf3-73b8-aa6a-4341d5160db0', 'member-01a1b71a-9ced-70cb-9238-7b4a1bbd8f0b@upgrade.theca', false, true, '2026-11-07 17:42:17.072462+00', NULL);


--
-- Data for Name: chunk; Type: TABLE DATA; Schema: public; Owner: -
--

INSERT INTO public.chunk OVERRIDING SYSTEM VALUE VALUES (1, '01a1b71a-9e16-71f8-adb6-a8b498453d75', 1, 1, '\x54c8fd1b845a52c662e3e2fdc652655d', 1, 17, 17, 'none', '\x4173e2e9472711064240a301796493fb5bcea5f03d5223aadc572651a6ed402c', 1, '2026-11-07 17:58:17.391127+00', '2026-11-07 17:42:17.406539+00', '2026-11-07 17:42:17.406539+00', NULL, DEFAULT);
INSERT INTO public.chunk OVERRIDING SYSTEM VALUE VALUES (2, '01a1b71a-9eb0-7d2d-8b21-b0ea6e530db1', 1, 1, '\x5256cbbd9e0c17947e93696c1df3e642', 1, 17, 17, 'none', '\x0a2955956b327975ee23e7d09d679aba5ec71cd43f94f961cd70f72b722c85ef', 2, '2026-11-07 17:58:17.546787+00', '2026-11-07 17:42:17.558155+00', '2026-11-07 17:42:17.558155+00', NULL, DEFAULT);
INSERT INTO public.chunk OVERRIDING SYSTEM VALUE VALUES (3, '01a1b71a-9f1e-7356-85ab-bec9601ffe97', 1, 1, '\x2d1b96824c56bcb968863cff76331dcd', 1, 10, 10, 'none', '\x56739bb8b5fd8c5cc0d9c0ea284350e858eb3083884dd5357494c3a1374d85cf', 2, '2026-11-07 17:58:17.655554+00', '2026-11-07 17:42:17.662809+00', '2026-11-07 17:42:17.662809+00', NULL, DEFAULT);
INSERT INTO public.chunk OVERRIDING SYSTEM VALUE VALUES (4, '01a1b71a-9f93-70e3-90ef-9f9c598f547b', 1, 1, '\x7823072e77829c306842b1f39a4eaf8c', 1, 5, 5, 'none', '\x7763c8ca8f464e6eb9f819d7c77f68ec682c592024646d3b239f449a2d86e1e2', 1, '2026-11-07 17:58:17.776778+00', '2026-11-07 17:42:17.786136+00', '2026-11-07 17:42:17.786136+00', NULL, DEFAULT);
INSERT INTO public.chunk OVERRIDING SYSTEM VALUE VALUES (5, '01a1b71a-a00b-75d1-8a63-9b5c1aca730a', 1, 2, '\x995a320da7c99a93fc6617f1d471e4f8', 1, 7, 7, 'none', '\x4c8072c6e5cc5a31e00ad1b50be66763ff6847fbbf8fb5507ff62a650500f56b', 1, '2026-11-07 17:58:17.892432+00', '2026-11-07 17:42:17.902391+00', '2026-11-07 17:42:17.902391+00', NULL, DEFAULT);


--
-- Data for Name: chunk_in_version; Type: TABLE DATA; Schema: public; Owner: -
--

INSERT INTO public.chunk_in_version OVERRIDING SYSTEM VALUE VALUES (1, '01a1b71a-9e6c-7f8a-910a-211bd1b160b0', 1, 1, 1, true, 0);
INSERT INTO public.chunk_in_version OVERRIDING SYSTEM VALUE VALUES (2, '01a1b71a-9ef8-765a-91ba-2c8147a730fb', 1, 2, 2, true, 0);
INSERT INTO public.chunk_in_version OVERRIDING SYSTEM VALUE VALUES (3, '01a1b71a-9f63-75d8-9bf1-87bebde96396', 1, 3, 3, true, 0);
INSERT INTO public.chunk_in_version OVERRIDING SYSTEM VALUE VALUES (4, '01a1b71a-9fe5-7254-861b-57b439b3aa49', 1, 4, 4, true, 0);
INSERT INTO public.chunk_in_version OVERRIDING SYSTEM VALUE VALUES (5, '01a1b71a-a059-789d-84e1-60061bde402b', 1, 5, 5, true, 0);


--
-- Data for Name: email; Type: TABLE DATA; Schema: public; Owner: -
--

INSERT INTO public.email OVERRIDING SYSTEM VALUE VALUES (1, '01a1b71a-9cf5-7e93-b10b-c9663809dbf9', 'member-01a1b71a-9ced-70cb-9238-7b4a1bbd8f0b@upgrade.theca', 'invitation', '{"user": "01a1b71a-9cf4-7195-9dd5-ecee482af6f9", "inviter": "admin@upgrade.theca", "template": "invitation", "organization": "Acme"}', 1, '2026-11-07 17:42:17.072462+00', '2026-11-07 17:42:17.087131+00', NULL, NULL);


--
-- Data for Name: event; Type: TABLE DATA; Schema: public; Owner: -
--

INSERT INTO public.event OVERRIDING SYSTEM VALUE VALUES (1, 1, 1, '01a1b71a-9e5f-76b2-b105-1edf001b40e0', 'node', 'created', 1, '2026-11-07 17:42:17.437479+00', '01a1b71a-9e74-72d8-88cb-b8da31109f57');
INSERT INTO public.event OVERRIDING SYSTEM VALUE VALUES (2, 1, 2, '01a1b71a-9e5f-76b2-b105-1edf001b40e0', 'node', 'versioned', 2, '2026-11-07 17:42:17.584198+00', '01a1b71a-9efa-7520-9daa-0dfa74815e3e');
INSERT INTO public.event OVERRIDING SYSTEM VALUE VALUES (3, 1, 3, '01a1b71a-9f01-7fcc-9e8c-a70d78618df5', 'node', 'created', 1, '2026-11-07 17:42:17.60135+00', '01a1b71a-9f02-7914-8b1f-1b92f9a30489');
INSERT INTO public.event OVERRIDING SYSTEM VALUE VALUES (4, 1, 4, '01a1b71a-9f5c-7046-844e-5b580e2d448a', 'node', 'created', 2, '2026-11-07 17:42:17.689981+00', '01a1b71a-9f64-7d83-803c-4aa55a81311d');
INSERT INTO public.event OVERRIDING SYSTEM VALUE VALUES (5, 1, 5, '01a1b71a-9fdb-73cb-a1b1-64a17a64c37e', 'node', 'created', 1, '2026-11-07 17:42:17.815492+00', '01a1b71a-9fe6-7c0c-b083-fad1920cb92f');
INSERT INTO public.event OVERRIDING SYSTEM VALUE VALUES (6, 1, 6, '01a1b71a-9fdb-73cb-a1b1-64a17a64c37e', 'node', 'trashed', 1, '2026-11-07 17:42:17.835742+00', '01a1b71a-9fec-7d2b-95af-91cab3df1148');
INSERT INTO public.event OVERRIDING SYSTEM VALUE VALUES (7, 2, 1, '01a1b71a-a04c-70f9-8817-707f8825dfe3', 'node', 'created', 1, '2026-11-07 17:42:17.931025+00', '01a1b71a-a05b-7e11-b394-296ac96425d3');


--
-- Data for Name: identity; Type: TABLE DATA; Schema: public; Owner: -
--

INSERT INTO public.identity OVERRIDING SYSTEM VALUE VALUES (1, '01a1b71a-9cdb-7aad-847e-af5b7ef924fd', 1, 'password', NULL, '{"hash": "$argon2id$v=19$m=19456,t=2,p=1$oxumOjoydaCMDIo7Qou0iQ$/ZM9xpFzUG+z0MEYe6+UZa8xzeZVkXWxtGgCtMqxXCk"}');
INSERT INTO public.identity OVERRIDING SYSTEM VALUE VALUES (2, '01a1b71a-9d86-7807-a438-6e4e7910e33e', 2, 'password', NULL, '{"hash": "$argon2id$v=19$m=19456,t=2,p=1$hN23JRemc9UzLKmehI+a7w$WdOZRfGs3rJteh3mLkmNzN1pSlHAZKGb+nnWohbJTb0"}');
INSERT INTO public.identity OVERRIDING SYSTEM VALUE VALUES (3, '01a1b71a-a062-7f5e-bfe9-e623f9cc715b', 2, 'totp', NULL, '{"secret": "BQGf/FUV7HQEaL2m3jBJhkU1/LSJ9P1K2Sxrjn23KoVzegGUpD7QUcvNsIqWUTqiymViaELGdtqzMi4y", "confirmed": true, "last_step": 59802443}');


--
-- Data for Name: identity_provider; Type: TABLE DATA; Schema: public; Owner: -
--



--
-- Data for Name: job_run; Type: TABLE DATA; Schema: public; Owner: -
--

INSERT INTO public.job_run OVERRIDING SYSTEM VALUE VALUES (3, '01a1b71a-9c65-77d1-8113-86bfb81e7c3a', 'orphan_check', '2026-11-07 17:42:16.933348+00', '2026-11-07 17:42:16.937288+00', 'success', 0, NULL);
INSERT INTO public.job_run OVERRIDING SYSTEM VALUE VALUES (5, '01a1b71a-9c6b-7d22-a033-3782feabd08c', 'email_send', '2026-11-07 17:42:16.939549+00', '2026-11-07 17:42:16.942094+00', 'success', 0, NULL);
INSERT INTO public.job_run OVERRIDING SYSTEM VALUE VALUES (4, '01a1b71a-9c6a-7852-aad8-b2a574acf3ba', 'chunk_gc', '2026-11-07 17:42:16.938154+00', '2026-11-07 17:42:16.942467+00', 'success', 0, NULL);
INSERT INTO public.job_run OVERRIDING SYSTEM VALUE VALUES (2, '01a1b71a-9c64-7f55-831a-c37fc9750a71', 'cleanup', '2026-11-07 17:42:16.932834+00', '2026-11-07 17:42:16.946667+00', 'success', 0, NULL);
INSERT INTO public.job_run OVERRIDING SYSTEM VALUE VALUES (1, '01a1b71a-9c60-7bc0-8f79-b02a5b504c38', 'trash_purge', '2026-11-07 17:42:16.928649+00', '2026-11-07 17:42:16.966015+00', 'success', 0, NULL);
INSERT INTO public.job_run OVERRIDING SYSTEM VALUE VALUES (6, '01a1b71a-9cfb-7896-bd79-8702cbd3c3bc', 'email_send', '2026-11-07 17:42:17.08347+00', '2026-11-07 17:42:17.116021+00', 'success', 1, NULL);


--
-- Data for Name: library; Type: TABLE DATA; Schema: public; Owner: -
--

INSERT INTO public.library OVERRIDING SYSTEM VALUE VALUES (1, '01a1b71a-9ddb-78ca-b861-36beee0344d4', 'Docs', 1, 1, NULL, 6);
INSERT INTO public.library OVERRIDING SYSTEM VALUE VALUES (2, '01a1b71a-9de8-710e-a220-41d4c05d22c8', 'Photos', 1, 1, NULL, 1);


--
-- Data for Name: library_member; Type: TABLE DATA; Schema: public; Owner: -
--

INSERT INTO public.library_member OVERRIDING SYSTEM VALUE VALUES (1, '01a1b71a-9ddf-7aea-b7e1-b12c0ffa1101', 1, 1, 1, 'write');
INSERT INTO public.library_member OVERRIDING SYSTEM VALUE VALUES (2, '01a1b71a-9de9-7b53-880a-41e69d7c1a2c', 1, 2, 1, 'write');
INSERT INTO public.library_member OVERRIDING SYSTEM VALUE VALUES (3, '01a1b71a-9dee-76ae-8002-92d85a66ffae', 1, 1, 2, 'write');


--
-- Data for Name: library_namespace; Type: TABLE DATA; Schema: public; Owner: -
--

INSERT INTO public.library_namespace OVERRIDING SYSTEM VALUE VALUES (1, '01a1b71a-9de1-7285-aa93-5df60ddbdef7', 1, 1, 1);


--
-- Data for Name: login_flow; Type: TABLE DATA; Schema: public; Owner: -
--



--
-- Data for Name: namespace; Type: TABLE DATA; Schema: public; Owner: -
--

INSERT INTO public.namespace OVERRIDING SYSTEM VALUE VALUES (1, '01a1b71a-9dce-77ce-a8fd-467b16f4d697', 1, 'Shared');


--
-- Data for Name: node; Type: TABLE DATA; Schema: public; Owner: -
--

INSERT INTO public.node OVERRIDING SYSTEM VALUE VALUES (1, '01a1b71a-9ddd-781d-aaf9-481b9449f5bb', 1, 1, NULL, false, '', false, NULL, NULL);
INSERT INTO public.node OVERRIDING SYSTEM VALUE VALUES (2, '01a1b71a-9de9-710b-a0c8-db33ac53cb03', 1, 2, NULL, false, '', false, NULL, NULL);
INSERT INTO public.node OVERRIDING SYSTEM VALUE VALUES (3, '01a1b71a-9e5f-76b2-b105-1edf001b40e0', 1, 1, 1, false, 'readme.txt', true, 2, NULL);
INSERT INTO public.node OVERRIDING SYSTEM VALUE VALUES (4, '01a1b71a-9f01-7fcc-9e8c-a70d78618df5', 1, 1, 1, false, 'notes', false, NULL, NULL);
INSERT INTO public.node OVERRIDING SYSTEM VALUE VALUES (5, '01a1b71a-9f5c-7046-844e-5b580e2d448a', 1, 1, 4, false, 'plan.txt', true, 3, NULL);
INSERT INTO public.node OVERRIDING SYSTEM VALUE VALUES (6, '01a1b71a-9fdb-73cb-a1b1-64a17a64c37e', 1, 1, 1, false, 'old.txt', true, 4, '2026-11-07 17:42:17.835742+00');
INSERT INTO public.node OVERRIDING SYSTEM VALUE VALUES (7, '01a1b71a-a04c-70f9-8817-707f8825dfe3', 1, 2, 2, false, 'cat.txt', true, 5, NULL);


--
-- Data for Name: organization; Type: TABLE DATA; Schema: public; Owner: -
--

INSERT INTO public.organization OVERRIDING SYSTEM VALUE VALUES (1, '01a1b71a-9cdc-7db3-af0e-0a6dfbd63e11', 'Acme', NULL, 1, NULL, true, NULL, NULL, NULL);


--
-- Data for Name: setting; Type: TABLE DATA; Schema: public; Owner: -
--

INSERT INTO public.setting VALUES ('smtp_from', 'Theca <theca@upgrade.theca>');
INSERT INTO public.setting VALUES ('smtp_host', 'localhost');
INSERT INTO public.setting VALUES ('smtp_port', '1025');
INSERT INTO public.setting VALUES ('smtp_tls', 'none');
INSERT INTO public.setting VALUES ('trash_retention_secs', '315360000');


--
-- Data for Name: storage; Type: TABLE DATA; Schema: public; Owner: -
--

INSERT INTO public.storage OVERRIDING SYSTEM VALUE VALUES (1, '01a1b71a-9cde-7f16-ae80-ae85537c714f', NULL, 'main', 'filesystem', '{"path": "files"}', true);


--
-- Data for Name: token; Type: TABLE DATA; Schema: public; Owner: -
--

INSERT INTO public.token OVERRIDING SYSTEM VALUE VALUES (1, '01a1b71a-9ce2-771e-8241-e646ae4a1d74', 'session', '01a1b71a-9cda-7f58-bdd0-ca4ab9a00ad5', 'th_fe3e776d0a92a94e8cacb179', '\x94a6d1610b31ddf1cb286a5e7adc34d7e1f3d4889bf0dc071fe130352684ab94', NULL, '127.0.0.1', NULL, '2026-11-07 17:42:17.058379+00', '2026-11-07 17:42:17.058379+00', NULL, NULL, 'Unknown device', NULL, NULL, false);
INSERT INTO public.token OVERRIDING SYSTEM VALUE VALUES (2, '01a1b71a-9d00-7517-9496-c6e933de9946', 'invitation', '01a1b71a-9cf4-7195-9dd5-ecee482af6f9', 'th_be455b459e4f85ca6c6d047f', '\x54745a23aae1cf3b702552e7f3d58621de17db73f74a3745e3120c45c9658df9', NULL, NULL, NULL, '2026-11-07 17:42:17.087131+00', NULL, '2026-11-07 17:42:17.198832+00', 'used', 'invitation', NULL, NULL, false);
INSERT INTO public.token OVERRIDING SYSTEM VALUE VALUES (3, '01a1b71a-9d8b-75c1-958a-18c81e5086ad', 'session', '01a1b71a-9cf3-73b8-aa6a-4341d5160db0', 'th_7cb301ca7d6a4000d42e9a7e', '\x853da61c2dfd4ae330ad341671434b2012429f3a4b75692ed914437033f8ced6', NULL, '127.0.0.1', NULL, '2026-11-07 17:42:17.227295+00', '2026-11-07 17:42:17.227295+00', NULL, NULL, 'Unknown device', NULL, NULL, false);
INSERT INTO public.token OVERRIDING SYSTEM VALUE VALUES (4, '01a1b71a-9dc2-76c8-83a3-4a5021f0113f', 'session', '01a1b71a-9cf3-73b8-aa6a-4341d5160db0', 'th_3cf1ae804225a6df15d996bb', '\x94c10e4d58cd9ee2803fb588167f05095ca315da1f7c35ff2ff9e00e88faae3b', NULL, '127.0.0.1', NULL, '2026-11-07 17:42:17.282334+00', '2026-11-07 17:42:17.282334+00', NULL, NULL, 'Unknown device', NULL, NULL, false);
INSERT INTO public.token OVERRIDING SYSTEM VALUE VALUES (5, '01a1b71a-a06d-7ff3-93e5-b1b17cdbc193', 'recovery_code', '01a1b71a-9cf3-73b8-aa6a-4341d5160db0', 'rc_7ad900ff1b7e09d3e5ef4d14', '\x7ad900ff1b7e09d3e5ef4d1422ab439ae50cfd114fc728d2f8f8191a01ccd803', NULL, NULL, NULL, '2026-11-07 17:42:17.963608+00', NULL, NULL, NULL, 'recovery code', NULL, NULL, false);
INSERT INTO public.token OVERRIDING SYSTEM VALUE VALUES (6, '01a1b71a-a06e-721d-94f9-d8aed157ce50', 'recovery_code', '01a1b71a-9cf3-73b8-aa6a-4341d5160db0', 'rc_eeefd415b3e299f7d1df3a65', '\xeeefd415b3e299f7d1df3a6568327bf2be08bd3fb0171c919ab3c6fb4832d37a', NULL, NULL, NULL, '2026-11-07 17:42:17.963608+00', NULL, NULL, NULL, 'recovery code', NULL, NULL, false);
INSERT INTO public.token OVERRIDING SYSTEM VALUE VALUES (7, '01a1b71a-a06e-7237-a2a4-e0941f0c6b97', 'recovery_code', '01a1b71a-9cf3-73b8-aa6a-4341d5160db0', 'rc_048bac742bde50ff98d44fb8', '\x048bac742bde50ff98d44fb8810c4648a5e317f6cf67e7599b88a90672199cd8', NULL, NULL, NULL, '2026-11-07 17:42:17.963608+00', NULL, NULL, NULL, 'recovery code', NULL, NULL, false);
INSERT INTO public.token OVERRIDING SYSTEM VALUE VALUES (8, '01a1b71a-a06e-7242-abf1-3bb395577b00', 'recovery_code', '01a1b71a-9cf3-73b8-aa6a-4341d5160db0', 'rc_bd4dbe3f80295bfe1e1d90c6', '\xbd4dbe3f80295bfe1e1d90c681e9e9221f55bb6a7505135ccdd23911828b265f', NULL, NULL, NULL, '2026-11-07 17:42:17.963608+00', NULL, NULL, NULL, 'recovery code', NULL, NULL, false);
INSERT INTO public.token OVERRIDING SYSTEM VALUE VALUES (9, '01a1b71a-a06e-724c-ad01-1bdf6335ee80', 'recovery_code', '01a1b71a-9cf3-73b8-aa6a-4341d5160db0', 'rc_4b53d47a56e0845f49a05f47', '\x4b53d47a56e0845f49a05f4758fa50a5fdd003bf721295d6a7bb07d6484b933c', NULL, NULL, NULL, '2026-11-07 17:42:17.963608+00', NULL, NULL, NULL, 'recovery code', NULL, NULL, false);
INSERT INTO public.token OVERRIDING SYSTEM VALUE VALUES (10, '01a1b71a-a06e-7254-8a5c-5af5a68b559c', 'recovery_code', '01a1b71a-9cf3-73b8-aa6a-4341d5160db0', 'rc_01f9f4ed577f37f6ce96399f', '\x01f9f4ed577f37f6ce96399f16b18dbf33a48d291950b495e416f1fadfb886e4', NULL, NULL, NULL, '2026-11-07 17:42:17.963608+00', NULL, NULL, NULL, 'recovery code', NULL, NULL, false);
INSERT INTO public.token OVERRIDING SYSTEM VALUE VALUES (11, '01a1b71a-a06e-725d-88e8-80eb10939da6', 'recovery_code', '01a1b71a-9cf3-73b8-aa6a-4341d5160db0', 'rc_0b41f89be4c8597c9705ac17', '\x0b41f89be4c8597c9705ac17e23801281fee773779f7f383505504e2414cb53f', NULL, NULL, NULL, '2026-11-07 17:42:17.963608+00', NULL, NULL, NULL, 'recovery code', NULL, NULL, false);
INSERT INTO public.token OVERRIDING SYSTEM VALUE VALUES (12, '01a1b71a-a06e-7265-822f-a56c36da6ac9', 'recovery_code', '01a1b71a-9cf3-73b8-aa6a-4341d5160db0', 'rc_e7b5276409294a00360a5751', '\xe7b5276409294a00360a575191b67ff9e35bf3cd0c7662528813675aff45fe88', NULL, NULL, NULL, '2026-11-07 17:42:17.963608+00', NULL, NULL, NULL, 'recovery code', NULL, NULL, false);
INSERT INTO public.token OVERRIDING SYSTEM VALUE VALUES (13, '01a1b71a-a06e-726d-a55d-d118c12d2559', 'recovery_code', '01a1b71a-9cf3-73b8-aa6a-4341d5160db0', 'rc_4021f7b80b442f5291501b11', '\x4021f7b80b442f5291501b114a23eb04334ad4d33beb4e4255392fd6169d85d9', NULL, NULL, NULL, '2026-11-07 17:42:17.963608+00', NULL, NULL, NULL, 'recovery code', NULL, NULL, false);
INSERT INTO public.token OVERRIDING SYSTEM VALUE VALUES (14, '01a1b71a-a06e-7293-8e1a-a676a0847afe', 'recovery_code', '01a1b71a-9cf3-73b8-aa6a-4341d5160db0', 'rc_01a4d0dfcccb122c013ce4b6', '\x01a4d0dfcccb122c013ce4b62f35365ba0cfad9adb8a99089ed6f92797a2489f', NULL, NULL, NULL, '2026-11-07 17:42:17.963608+00', NULL, NULL, NULL, 'recovery code', NULL, NULL, false);
INSERT INTO public.token OVERRIDING SYSTEM VALUE VALUES (15, '01a1b71a-a071-73cd-827e-8b2c5d771ba0', 'app_password', '01a1b71a-9cda-7f58-bdd0-ca4ab9a00ad5', 'th_2f3335d42259f464f7d87fca', '\x6044c9dae524823dea899ff4789a367465ed7e5b41376a872252250a7f0a76c0', NULL, NULL, NULL, '2026-11-07 17:42:17.969152+00', NULL, NULL, NULL, 'upgrade test', NULL, NULL, false);


--
-- Data for Name: user; Type: TABLE DATA; Schema: public; Owner: -
--

INSERT INTO public."user" OVERRIDING SYSTEM VALUE VALUES (1, '01a1b71a-9cdd-7b1c-81b9-7fbd0c33ffb7', 1, 1, '2026-11-07 17:42:17.048881+00', NULL, DEFAULT);
INSERT INTO public."user" OVERRIDING SYSTEM VALUE VALUES (2, '01a1b71a-9cf4-7195-9dd5-ecee482af6f9', 2, 1, '2026-11-07 17:42:17.198832+00', NULL, DEFAULT);


--
-- Data for Name: version; Type: TABLE DATA; Schema: public; Owner: -
--

INSERT INTO public.version OVERRIDING SYSTEM VALUE VALUES (1, '01a1b71a-9e6a-72b3-bbb1-f090195e5782', 1, 3, true, NULL, 'first', 17, '2026-11-07 17:42:17.437479+00', '2026-11-07 17:42:17.437479+00', 1);
INSERT INTO public.version OVERRIDING SYSTEM VALUE VALUES (2, '01a1b71a-9ef7-7835-9b23-4f4d231b4a58', 1, 3, true, 1, 'second', 17, '2026-11-07 17:42:17.584198+00', '2026-11-07 17:42:17.584198+00', 2);
INSERT INTO public.version OVERRIDING SYSTEM VALUE VALUES (3, '01a1b71a-9f62-7d77-8084-2954ef0b683a', 1, 5, true, NULL, 'a plan', 10, '2026-11-07 17:42:17.689981+00', '2026-11-07 17:42:17.689981+00', 2);
INSERT INTO public.version OVERRIDING SYSTEM VALUE VALUES (4, '01a1b71a-9fe4-7932-ad7d-274b3b183462', 1, 6, true, NULL, 'soon gone', 5, '2026-11-07 17:42:17.815492+00', '2026-11-07 17:42:17.815492+00', 1);
INSERT INTO public.version OVERRIDING SYSTEM VALUE VALUES (5, '01a1b71a-a058-7556-bf2b-32b585e95b38', 1, 7, true, NULL, 'meow', 7, '2026-11-07 17:42:17.931025+00', '2026-11-07 17:42:17.931025+00', 1);


--
-- Name: account_pk_seq; Type: SEQUENCE SET; Schema: public; Owner: -
--

SELECT pg_catalog.setval('public.account_pk_seq', 2, true);


--
-- Name: chunk_in_version_pk_seq; Type: SEQUENCE SET; Schema: public; Owner: -
--

SELECT pg_catalog.setval('public.chunk_in_version_pk_seq', 5, true);


--
-- Name: chunk_pk_seq; Type: SEQUENCE SET; Schema: public; Owner: -
--

SELECT pg_catalog.setval('public.chunk_pk_seq', 5, true);


--
-- Name: email_pk_seq; Type: SEQUENCE SET; Schema: public; Owner: -
--

SELECT pg_catalog.setval('public.email_pk_seq', 1, true);


--
-- Name: event_pk_seq; Type: SEQUENCE SET; Schema: public; Owner: -
--

SELECT pg_catalog.setval('public.event_pk_seq', 7, true);


--
-- Name: identity_pk_seq; Type: SEQUENCE SET; Schema: public; Owner: -
--

SELECT pg_catalog.setval('public.identity_pk_seq', 3, true);


--
-- Name: identity_provider_pk_seq; Type: SEQUENCE SET; Schema: public; Owner: -
--

SELECT pg_catalog.setval('public.identity_provider_pk_seq', 1, false);


--
-- Name: job_run_pk_seq; Type: SEQUENCE SET; Schema: public; Owner: -
--

SELECT pg_catalog.setval('public.job_run_pk_seq', 6, true);


--
-- Name: library_member_pk_seq; Type: SEQUENCE SET; Schema: public; Owner: -
--

SELECT pg_catalog.setval('public.library_member_pk_seq', 3, true);


--
-- Name: library_namespace_pk_seq; Type: SEQUENCE SET; Schema: public; Owner: -
--

SELECT pg_catalog.setval('public.library_namespace_pk_seq', 1, true);


--
-- Name: library_pk_seq; Type: SEQUENCE SET; Schema: public; Owner: -
--

SELECT pg_catalog.setval('public.library_pk_seq', 2, true);


--
-- Name: login_flow_pk_seq; Type: SEQUENCE SET; Schema: public; Owner: -
--

SELECT pg_catalog.setval('public.login_flow_pk_seq', 1, false);


--
-- Name: namespace_pk_seq; Type: SEQUENCE SET; Schema: public; Owner: -
--

SELECT pg_catalog.setval('public.namespace_pk_seq', 1, true);


--
-- Name: node_pk_seq; Type: SEQUENCE SET; Schema: public; Owner: -
--

SELECT pg_catalog.setval('public.node_pk_seq', 7, true);


--
-- Name: organization_pk_seq; Type: SEQUENCE SET; Schema: public; Owner: -
--

SELECT pg_catalog.setval('public.organization_pk_seq', 1, true);


--
-- Name: storage_pk_seq; Type: SEQUENCE SET; Schema: public; Owner: -
--

SELECT pg_catalog.setval('public.storage_pk_seq', 1, true);


--
-- Name: token_pk_seq; Type: SEQUENCE SET; Schema: public; Owner: -
--

SELECT pg_catalog.setval('public.token_pk_seq', 15, true);


--
-- Name: user_pk_seq; Type: SEQUENCE SET; Schema: public; Owner: -
--

SELECT pg_catalog.setval('public.user_pk_seq', 2, true);


--
-- Name: version_pk_seq; Type: SEQUENCE SET; Schema: public; Owner: -
--

SELECT pg_catalog.setval('public.version_pk_seq', 5, true);


--
-- Name: _sqlx_migrations _sqlx_migrations_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public._sqlx_migrations
    ADD CONSTRAINT _sqlx_migrations_pkey PRIMARY KEY (version);


--
-- Name: account account_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account
    ADD CONSTRAINT account_pkey PRIMARY KEY (pk);


--
-- Name: account account_uuid_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account
    ADD CONSTRAINT account_uuid_key UNIQUE (uuid);


--
-- Name: chunk_in_version chunk_in_version_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.chunk_in_version
    ADD CONSTRAINT chunk_in_version_pkey PRIMARY KEY (pk);


--
-- Name: chunk_in_version chunk_in_version_uuid_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.chunk_in_version
    ADD CONSTRAINT chunk_in_version_uuid_key UNIQUE (uuid);


--
-- Name: chunk_in_version chunk_in_version_version_position_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.chunk_in_version
    ADD CONSTRAINT chunk_in_version_version_position_key UNIQUE (version, "position");


--
-- Name: chunk chunk_organization_pk_ready_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.chunk
    ADD CONSTRAINT chunk_organization_pk_ready_key UNIQUE (organization, pk, ready);


--
-- Name: chunk chunk_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.chunk
    ADD CONSTRAINT chunk_pkey PRIMARY KEY (pk);


--
-- Name: chunk chunk_uuid_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.chunk
    ADD CONSTRAINT chunk_uuid_key UNIQUE (uuid);


--
-- Name: email email_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.email
    ADD CONSTRAINT email_pkey PRIMARY KEY (pk);


--
-- Name: email email_uuid_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.email
    ADD CONSTRAINT email_uuid_key UNIQUE (uuid);


--
-- Name: event event_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.event
    ADD CONSTRAINT event_pkey PRIMARY KEY (pk);


--
-- Name: event event_seq_unique; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.event
    ADD CONSTRAINT event_seq_unique UNIQUE (library, seq);


--
-- Name: identity identity_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.identity
    ADD CONSTRAINT identity_pkey PRIMARY KEY (pk);


--
-- Name: identity_provider identity_provider_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.identity_provider
    ADD CONSTRAINT identity_provider_pkey PRIMARY KEY (pk);


--
-- Name: identity_provider identity_provider_uuid_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.identity_provider
    ADD CONSTRAINT identity_provider_uuid_key UNIQUE (uuid);


--
-- Name: identity identity_uuid_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.identity
    ADD CONSTRAINT identity_uuid_key UNIQUE (uuid);


--
-- Name: job_run job_run_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.job_run
    ADD CONSTRAINT job_run_pkey PRIMARY KEY (pk);


--
-- Name: job_run job_run_uuid_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.job_run
    ADD CONSTRAINT job_run_uuid_key UNIQUE (uuid);


--
-- Name: library_member library_member_library_user_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.library_member
    ADD CONSTRAINT library_member_library_user_key UNIQUE (library, "user");


--
-- Name: library_member library_member_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.library_member
    ADD CONSTRAINT library_member_pkey PRIMARY KEY (pk);


--
-- Name: library_member library_member_uuid_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.library_member
    ADD CONSTRAINT library_member_uuid_key UNIQUE (uuid);


--
-- Name: library_namespace library_namespace_library_namespace_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.library_namespace
    ADD CONSTRAINT library_namespace_library_namespace_key UNIQUE (library, namespace);


--
-- Name: library_namespace library_namespace_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.library_namespace
    ADD CONSTRAINT library_namespace_pkey PRIMARY KEY (pk);


--
-- Name: library_namespace library_namespace_uuid_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.library_namespace
    ADD CONSTRAINT library_namespace_uuid_key UNIQUE (uuid);


--
-- Name: library library_owner_pk_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.library
    ADD CONSTRAINT library_owner_pk_key UNIQUE (owner, pk);


--
-- Name: library library_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.library
    ADD CONSTRAINT library_pkey PRIMARY KEY (pk);


--
-- Name: library library_uuid_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.library
    ADD CONSTRAINT library_uuid_key UNIQUE (uuid);


--
-- Name: login_flow login_flow_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.login_flow
    ADD CONSTRAINT login_flow_pkey PRIMARY KEY (pk);


--
-- Name: login_flow login_flow_poll_hash_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.login_flow
    ADD CONSTRAINT login_flow_poll_hash_key UNIQUE (poll_hash);


--
-- Name: login_flow login_flow_uuid_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.login_flow
    ADD CONSTRAINT login_flow_uuid_key UNIQUE (uuid);


--
-- Name: namespace namespace_organization_pk_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.namespace
    ADD CONSTRAINT namespace_organization_pk_key UNIQUE (organization, pk);


--
-- Name: namespace namespace_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.namespace
    ADD CONSTRAINT namespace_pkey PRIMARY KEY (pk);


--
-- Name: namespace namespace_uuid_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.namespace
    ADD CONSTRAINT namespace_uuid_key UNIQUE (uuid);


--
-- Name: node node_organization_pk_is_file_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.node
    ADD CONSTRAINT node_organization_pk_is_file_key UNIQUE (organization, pk, is_file);


--
-- Name: node node_pk_library_is_file_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.node
    ADD CONSTRAINT node_pk_library_is_file_key UNIQUE (pk, library, is_file);


--
-- Name: node node_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.node
    ADD CONSTRAINT node_pkey PRIMARY KEY (pk);


--
-- Name: node node_uuid_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.node
    ADD CONSTRAINT node_uuid_key UNIQUE (uuid);


--
-- Name: organization organization_personal_of_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.organization
    ADD CONSTRAINT organization_personal_of_key UNIQUE (personal_of);


--
-- Name: organization organization_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.organization
    ADD CONSTRAINT organization_pkey PRIMARY KEY (pk);


--
-- Name: organization organization_uuid_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.organization
    ADD CONSTRAINT organization_uuid_key UNIQUE (uuid);


--
-- Name: setting setting_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.setting
    ADD CONSTRAINT setting_pkey PRIMARY KEY (key);


--
-- Name: storage storage_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.storage
    ADD CONSTRAINT storage_pkey PRIMARY KEY (pk);


--
-- Name: storage storage_uuid_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.storage
    ADD CONSTRAINT storage_uuid_key UNIQUE (uuid);


--
-- Name: token token_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.token
    ADD CONSTRAINT token_pkey PRIMARY KEY (pk);


--
-- Name: token token_prefix_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.token
    ADD CONSTRAINT token_prefix_key UNIQUE (prefix);


--
-- Name: token token_uuid_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.token
    ADD CONSTRAINT token_uuid_key UNIQUE (uuid);


--
-- Name: user user_account_organization_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public."user"
    ADD CONSTRAINT user_account_organization_key UNIQUE (account, organization);


--
-- Name: user user_organization_pk_joined_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public."user"
    ADD CONSTRAINT user_organization_pk_joined_key UNIQUE (organization, pk, joined);


--
-- Name: user user_organization_pk_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public."user"
    ADD CONSTRAINT user_organization_pk_key UNIQUE (organization, pk);


--
-- Name: user user_pk_account_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public."user"
    ADD CONSTRAINT user_pk_account_key UNIQUE (pk, account);


--
-- Name: user user_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public."user"
    ADD CONSTRAINT user_pkey PRIMARY KEY (pk);


--
-- Name: user user_uuid_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public."user"
    ADD CONSTRAINT user_uuid_key UNIQUE (uuid);


--
-- Name: version version_file_pk_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.version
    ADD CONSTRAINT version_file_pk_key UNIQUE (file, pk);


--
-- Name: version version_organization_pk_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.version
    ADD CONSTRAINT version_organization_pk_key UNIQUE (organization, pk);


--
-- Name: version version_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.version
    ADD CONSTRAINT version_pkey PRIMARY KEY (pk);


--
-- Name: version version_uuid_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.version
    ADD CONSTRAINT version_uuid_key UNIQUE (uuid);


--
-- Name: account_email; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX account_email ON public.account USING btree (lower(email));


--
-- Name: chunk_declared_by; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX chunk_declared_by ON public.chunk USING btree (declared_by) WHERE ((uploaded_at IS NULL) AND (declared_by IS NOT NULL));


--
-- Name: chunk_hash; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX chunk_hash ON public.chunk USING btree (organization, hash);


--
-- Name: chunk_in_version_chunk; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX chunk_in_version_chunk ON public.chunk_in_version USING btree (chunk);


--
-- Name: email_due; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX email_due ON public.email USING btree (next_attempt_at) WHERE ((sent_at IS NULL) AND (given_up_at IS NULL));


--
-- Name: event_actor; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX event_actor ON public.event USING btree (actor);


--
-- Name: identity_account; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX identity_account ON public.identity USING btree (account);


--
-- Name: identity_oidc; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX identity_oidc ON public.identity USING btree (provider, ((details ->> 'subject'::text))) WHERE (kind = 'oidc'::text);


--
-- Name: identity_password; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX identity_password ON public.identity USING btree (account) WHERE (kind = 'password'::text);


--
-- Name: identity_provider_name; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX identity_provider_name ON public.identity_provider USING btree (organization, lower(name)) NULLS NOT DISTINCT;


--
-- Name: identity_totp; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX identity_totp ON public.identity USING btree (account) WHERE (kind = 'totp'::text);


--
-- Name: library_member_user; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX library_member_user ON public.library_member USING btree ("user");


--
-- Name: library_name; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX library_name ON public.library USING btree (owner, lower(name)) WHERE (deleted_at IS NULL);


--
-- Name: library_namespace_namespace; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX library_namespace_namespace ON public.library_namespace USING btree (namespace);


--
-- Name: login_flow_account; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX login_flow_account ON public.login_flow USING btree (account);


--
-- Name: namespace_name; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX namespace_name ON public.namespace USING btree (organization, lower(name));


--
-- Name: node_name; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX node_name ON public.node USING btree (parent, lower(name)) WHERE (deleted_at IS NULL);


--
-- Name: node_name_search; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX node_name_search ON public.node USING gin (name public.gin_trgm_ops);


--
-- Name: node_one_root; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX node_one_root ON public.node USING btree (library) WHERE (parent IS NULL);


--
-- Name: node_parent_all; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX node_parent_all ON public.node USING btree (parent);


--
-- Name: node_trash; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX node_trash ON public.node USING btree (library) WHERE (deleted_at IS NOT NULL);


--
-- Name: storage_name; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX storage_name ON public.storage USING btree (organization, lower(name)) NULLS NOT DISTINCT;


--
-- Name: storage_one_default; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX storage_one_default ON public.storage USING btree (is_default) WHERE is_default;


--
-- Name: token_resource; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX token_resource ON public.token USING btree (resource);


--
-- Name: version_by_author; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX version_by_author ON public.version USING btree (author);


--
-- Name: version_by_file; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX version_by_file ON public.version USING btree (file);


--
-- Name: chunk chunk_storage_organization; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER chunk_storage_organization BEFORE INSERT OR UPDATE OF storage, organization ON public.chunk FOR EACH ROW EXECUTE FUNCTION public.chunk_storage_organization();


--
-- Name: library library_storage_organization; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER library_storage_organization BEFORE INSERT OR UPDATE OF storage, owner ON public.library FOR EACH ROW EXECUTE FUNCTION public.library_storage_organization();


--
-- Name: node node_file_has_version; Type: TRIGGER; Schema: public; Owner: -
--

CREATE CONSTRAINT TRIGGER node_file_has_version AFTER INSERT OR UPDATE OF current_version ON public.node DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN (new.is_file) EXECUTE FUNCTION public.node_file_has_version();


--
-- Name: node node_no_cycle; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER node_no_cycle BEFORE UPDATE OF parent ON public.node FOR EACH ROW WHEN (((new.parent IS DISTINCT FROM old.parent) AND (new.parent IS NOT NULL))) EXECUTE FUNCTION public.node_no_cycle();


--
-- Name: organization organization_default_storage; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER organization_default_storage BEFORE INSERT OR UPDATE OF default_storage ON public.organization FOR EACH ROW EXECUTE FUNCTION public.organization_default_storage();


--
-- Name: storage storage_organization_immutable; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER storage_organization_immutable BEFORE UPDATE OF organization ON public.storage FOR EACH ROW EXECUTE FUNCTION public.storage_organization_immutable();


--
-- Name: user user_left_identities; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER user_left_identities AFTER DELETE ON public."user" FOR EACH ROW EXECUTE FUNCTION public.user_left_identities();


--
-- Name: user user_personal_organization; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER user_personal_organization BEFORE INSERT OR UPDATE OF account, organization ON public."user" FOR EACH ROW EXECUTE FUNCTION public.user_personal_organization();


--
-- Name: chunk_in_version chunk_in_version_chunk; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.chunk_in_version
    ADD CONSTRAINT chunk_in_version_chunk FOREIGN KEY (organization, chunk, chunk_ready) REFERENCES public.chunk(organization, pk, ready);


--
-- Name: chunk_in_version chunk_in_version_version; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.chunk_in_version
    ADD CONSTRAINT chunk_in_version_version FOREIGN KEY (organization, version) REFERENCES public.version(organization, pk);


--
-- Name: chunk chunk_organization_declared_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.chunk
    ADD CONSTRAINT chunk_organization_declared_by_fkey FOREIGN KEY (organization, declared_by) REFERENCES public."user"(organization, pk) ON DELETE SET NULL (declared_by);


--
-- Name: chunk chunk_organization_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.chunk
    ADD CONSTRAINT chunk_organization_fkey FOREIGN KEY (organization) REFERENCES public.organization(pk);


--
-- Name: chunk chunk_organization_library_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.chunk
    ADD CONSTRAINT chunk_organization_library_fkey FOREIGN KEY (organization, library) REFERENCES public.library(owner, pk) ON DELETE SET NULL (library);


--
-- Name: chunk chunk_storage_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.chunk
    ADD CONSTRAINT chunk_storage_fkey FOREIGN KEY (storage) REFERENCES public.storage(pk);


--
-- Name: event event_actor_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.event
    ADD CONSTRAINT event_actor_fkey FOREIGN KEY (actor) REFERENCES public.account(pk) ON DELETE SET NULL;


--
-- Name: event event_library_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.event
    ADD CONSTRAINT event_library_fkey FOREIGN KEY (library) REFERENCES public.library(pk) ON DELETE CASCADE;


--
-- Name: identity identity_account_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.identity
    ADD CONSTRAINT identity_account_fkey FOREIGN KEY (account) REFERENCES public.account(pk) ON DELETE CASCADE;


--
-- Name: identity identity_provider_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.identity
    ADD CONSTRAINT identity_provider_fkey FOREIGN KEY (provider) REFERENCES public.identity_provider(pk) ON DELETE CASCADE;


--
-- Name: identity_provider identity_provider_organization_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.identity_provider
    ADD CONSTRAINT identity_provider_organization_fkey FOREIGN KEY (organization) REFERENCES public.organization(pk) ON DELETE CASCADE;


--
-- Name: library_member library_member_library; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.library_member
    ADD CONSTRAINT library_member_library FOREIGN KEY (organization, library) REFERENCES public.library(owner, pk) ON DELETE CASCADE;


--
-- Name: library_member library_member_user; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.library_member
    ADD CONSTRAINT library_member_user FOREIGN KEY (organization, "user") REFERENCES public."user"(organization, pk) ON DELETE CASCADE;


--
-- Name: library_namespace library_namespace_library; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.library_namespace
    ADD CONSTRAINT library_namespace_library FOREIGN KEY (organization, library) REFERENCES public.library(owner, pk) ON DELETE CASCADE;


--
-- Name: library_namespace library_namespace_namespace; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.library_namespace
    ADD CONSTRAINT library_namespace_namespace FOREIGN KEY (organization, namespace) REFERENCES public.namespace(organization, pk) ON DELETE CASCADE;


--
-- Name: library library_owner_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.library
    ADD CONSTRAINT library_owner_fkey FOREIGN KEY (owner) REFERENCES public.organization(pk);


--
-- Name: library library_storage_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.library
    ADD CONSTRAINT library_storage_fkey FOREIGN KEY (storage) REFERENCES public.storage(pk);


--
-- Name: login_flow login_flow_account_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.login_flow
    ADD CONSTRAINT login_flow_account_fkey FOREIGN KEY (account) REFERENCES public.account(pk) ON DELETE CASCADE;


--
-- Name: namespace namespace_organization_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.namespace
    ADD CONSTRAINT namespace_organization_fkey FOREIGN KEY (organization) REFERENCES public.organization(pk);


--
-- Name: node node_current_version; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.node
    ADD CONSTRAINT node_current_version FOREIGN KEY (pk, current_version) REFERENCES public.version(file, pk) DEFERRABLE INITIALLY DEFERRED;


--
-- Name: node node_library; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.node
    ADD CONSTRAINT node_library FOREIGN KEY (organization, library) REFERENCES public.library(owner, pk);


--
-- Name: node node_parent; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.node
    ADD CONSTRAINT node_parent FOREIGN KEY (parent, library, parent_is_file) REFERENCES public.node(pk, library, is_file) ON UPDATE CASCADE;


--
-- Name: organization organization_default_storage_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.organization
    ADD CONSTRAINT organization_default_storage_fkey FOREIGN KEY (default_storage) REFERENCES public.storage(pk);


--
-- Name: organization organization_owner; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.organization
    ADD CONSTRAINT organization_owner FOREIGN KEY (pk, owner, owner_joined) REFERENCES public."user"(organization, pk, joined) DEFERRABLE INITIALLY DEFERRED;


--
-- Name: organization organization_personal_of; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.organization
    ADD CONSTRAINT organization_personal_of FOREIGN KEY (personal_of) REFERENCES public.account(pk);


--
-- Name: organization organization_personal_owner; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.organization
    ADD CONSTRAINT organization_personal_owner FOREIGN KEY (owner, personal_of) REFERENCES public."user"(pk, account) DEFERRABLE INITIALLY DEFERRED;


--
-- Name: storage storage_organization_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.storage
    ADD CONSTRAINT storage_organization_fkey FOREIGN KEY (organization) REFERENCES public.organization(pk);


--
-- Name: token token_library; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.token
    ADD CONSTRAINT token_library FOREIGN KEY (organization, library) REFERENCES public.library(owner, pk) ON DELETE CASCADE;


--
-- Name: token token_organization_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.token
    ADD CONSTRAINT token_organization_fkey FOREIGN KEY (organization) REFERENCES public.organization(pk) ON DELETE CASCADE;


--
-- Name: user user_account_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public."user"
    ADD CONSTRAINT user_account_fkey FOREIGN KEY (account) REFERENCES public.account(pk);


--
-- Name: user user_organization_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public."user"
    ADD CONSTRAINT user_organization_fkey FOREIGN KEY (organization) REFERENCES public.organization(pk);


--
-- Name: version version_author; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.version
    ADD CONSTRAINT version_author FOREIGN KEY (author) REFERENCES public.account(pk) ON DELETE SET NULL;


--
-- Name: version version_file; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.version
    ADD CONSTRAINT version_file FOREIGN KEY (organization, file, file_is_file) REFERENCES public.node(organization, pk, is_file);


--
-- Name: version version_parent; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.version
    ADD CONSTRAINT version_parent FOREIGN KEY (file, parent_version) REFERENCES public.version(file, pk);


--
-- PostgreSQL database dump complete
--

\unrestrict 21m7mnhE4BhamCaq0PWXbXdEToIzGVkUwOeOt32sGZqSarVmPD0z8TbV8lxFui6

