-- What one group's long-term memory currently holds. Read-only.
--   psql -U qqbot -d qqbot -v gid=424242 -f dump_memory.sql
\set QUIET on
\pset pager off
\set gid :gid

\echo '===== 规模 ====='
SELECT (SELECT count(*) FROM raw_event  WHERE group_id = :gid) AS 原始消息,
       (SELECT count(*) FROM alias      WHERE group_id = :gid
                                          AND status <> 'inactive')          AS 在用称呼,
       (SELECT count(*) FROM memory_fact WHERE group_id = :gid
                                          AND status = 'active'
                                          AND valid_to IS NULL)              AS 当前事实,
       (SELECT count(*) FROM episode    WHERE group_id = :gid
                                          AND status = 'active')             AS 情景,
       (SELECT count(*) FROM memory_candidate WHERE group_id = :gid
                                          AND status = 'rejected')           AS 被拒候选,
       (SELECT count(*) FROM memory_job WHERE status = 'pending')            AS 待办作业;

\echo ''
\echo '===== 关于群本身 ====='
SELECT f.predicate, f.object_value::text AS 内容,
       round(f.confidence::numeric, 2) AS 置信,
       (SELECT count(*) FROM memory_fact_evidence e
         WHERE e.fact_id = f.id AND e.relation = 'supports') AS 证据,
       to_char(f.last_confirmed_at, 'MM-DD') AS 最近确认
  FROM memory_fact f
  JOIN entity e ON e.id = f.subject_entity_id
 WHERE f.group_id = :gid AND f.status = 'active' AND f.valid_to IS NULL
   AND e.entity_type = 'group'
 ORDER BY f.predicate;

\echo ''
\echo '===== 关于人 ====='
SELECT COALESCE(
         (SELECT a.alias_text FROM alias a
           WHERE a.target_entity_id = f.subject_entity_id
             AND (a.group_id = :gid OR a.group_id IS NULL)
             AND a.status = 'confirmed' AND a.valid_to IS NULL
           ORDER BY a.confidence DESC, a.alias_text LIMIT 1),
         left(f.subject_entity_id::text, 8)) AS 谁,
       f.predicate, f.object_value::text AS 内容,
       round(f.confidence::numeric, 2) AS 置信,
       (SELECT count(*) FROM memory_fact_evidence e
         WHERE e.fact_id = f.id AND e.relation = 'supports') AS 证据,
       to_char(f.last_confirmed_at, 'MM-DD') AS 最近确认
  FROM memory_fact f
  JOIN entity en ON en.id = f.subject_entity_id
 WHERE f.group_id = :gid AND f.status = 'active' AND f.valid_to IS NULL
   AND en.entity_type = 'person'
 ORDER BY 谁, f.predicate;

\echo ''
\echo '===== 称呼（含未确认的候选） ====='
SELECT a.alias_text AS 称呼, a.alias_type AS 来源, a.status AS 状态,
       round(a.confidence::numeric, 2) AS 置信,
       (SELECT count(DISTINCT r.platform_user_id)
          FROM alias_evidence ae JOIN raw_event r ON r.id = ae.raw_event_id
         WHERE ae.alias_id = a.id) AS 多少人用过,
       to_char(a.last_used_at, 'MM-DD') AS 最近一次
  FROM alias a
 WHERE (a.group_id = :gid OR a.group_id IS NULL) AND a.status <> 'inactive'
 ORDER BY a.status, a.confidence DESC, a.alias_text;

\echo ''
\echo '===== 情景 ====='
SELECT to_char(ep.started_at, 'MM-DD') AS 日期, ep.episode_type AS 类型,
       ep.summary AS 摘要,
       round(ep.importance::numeric, 2) AS 重要度,
       (SELECT count(*) FROM episode_participant p WHERE p.episode_id = ep.id) AS 参与人,
       (SELECT count(*) FROM embedding_index ix
         WHERE ix.object_id = ep.id AND ix.object_type = 'episode') AS 有向量
  FROM episode ep
 WHERE ep.group_id = :gid AND ep.status = 'active'
 ORDER BY ep.importance DESC, ep.started_at DESC;

\echo ''
\echo '===== 已经忘掉的（最近 20 条） ====='
SELECT f.predicate, f.object_value::text AS 内容, f.status AS 结局,
       to_char(f.valid_to, 'MM-DD') AS 何时
  FROM memory_fact f
 WHERE f.group_id = :gid AND f.valid_to IS NOT NULL
 ORDER BY f.valid_to DESC LIMIT 20;

\echo ''
\echo '===== 被校验器拒绝的（最近 20 条） ====='
SELECT candidate_type AS 类型, reject_reason AS 原因,
       left(payload::text, 90) AS 内容,
       to_char(processed_at, 'MM-DD HH24:MI') AS 何时
  FROM memory_candidate
 WHERE group_id = :gid AND status = 'rejected'
 ORDER BY processed_at DESC LIMIT 20;
