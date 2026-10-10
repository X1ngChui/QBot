# Simplified Chinese catalog (Fluent). Same message ids and variables as en.ftl.
writing-language = Simplified Chinese
command-owner_only = 该操作需要 bot owner 权限。
command-outcome_unknown = 未能确认操作结果，可能已经生效，也可能没有，请先确认再重试。
command-unknown_account = 目标账号暂无资料，无法执行此操作。
command-mentions_exact = 需要准确 @ { $count } 个账号。
command-mentions_max = 最多只能 @ { $max } 个账号。
command-bad_number = 编号或页码须为 1 到 10000 的整数。用法见 /help。
command-linked_once = --linked 只能写一次。
command-unknown_option = 存在未知选项。发送 /help 查看当前语法。
command-own_accounts_only = 只能操作当前账号或已确认关联账号的资料。
command-duration_invalid = 时长格式无效，请使用 30m、12h 或 3d。
command-truncated = （已截断）
command-list_separator = 、
command-name_entry = { $text }（置信度 { $confidence }）
command-display_fallback = 账号 { $account }
command-display_member = 成员 { $number }
command-scope_exact = 精确账号
command-scope_linked = 关联身份
usage-who = 用法：/who [--linked] [@账号]
usage-name_list = 用法：/name [--linked] [@账号]
usage-name_edit = 用法：/name { $action } [--linked] [@账号] 称呼
usage-note =
    用法：/note [--linked] [@账号]
    /note add [@账号] 内容
    /note edit [--linked] [@账号] 编号 内容
    /note remove [--linked] [@账号] 编号
    /note clear [--linked] [@账号]
usage-group = 用法：/group
usage-runs = 用法：/runs [运行编号]
usage-logs = 用法：/logs [条数]
usage-link_confirm = 用法：/link confirm
usage-link_cancel = 用法：/link cancel
usage-link_issue = 用法：/link @另一个账号、/link confirm 或 /link cancel
usage-unlink = 用法：/unlink
usage-stats = 用法：/stats [global]
usage-top = 用法：/top [--linked] [数量]
usage-tasks_list = 用法：/tasks list [页码]
usage-tasks_one = 用法：/tasks { $action } UUID
usage-tasks_add = 用法：/tasks add (--at 时间 | --in 时长) -- 内容
usage-tasks_edit = 用法：/tasks edit UUID [--at 时间 | --in 时长] [-- 内容]
usage-members = 用法：/members
usage-block = 用法：/block add|remove [--linked] @账号 [时长]
usage-block_add = 用法：/block add [--linked] @账号 [30m|12h|3d]
usage-block_remove = 用法：/block remove [--linked] @账号
usage-mute = 用法：/mute [status|on|off]
usage-help = 用法：/help [指令名]
help-header = 可用指令：
help-category_profile = 我的资料
help-category_identity = 身份
help-category_group = 本群
help-category_admin = 管理
help-category_help = 帮助
help-entry = { $command }　{ $what }
help-owner_tag = （仅 owner）
help-footer = 详细用法：/help 指令名
help-unknown = 没有「{ $name }」这条指令。
help-access_member = 所有成员可用
help-access_owner = 仅 bot owner
help-detail =
    { $command }　{ $what }
    { $access }

    { $detail }
what-who = 查看成员的资料
detail-who =
    /who [--linked] [@账号]
    显示称呼、人工备注和从聊天中自动归纳的内容，各自分别编号。默认查看当前精确账号；--linked 包括关联账号。
what-name = 管理成员的称呼
detail-name =
    /name [--linked] [@账号]
    /name add [--linked] [@账号] 称呼
    /name remove [--linked] [@账号] 称呼
    称呼由证据支撑；手动添加的称呼视为已确认。--linked 作用于所有关联账号背后的同一个人。
what-note = 管理人工备注
detail-note =
    /note [--linked] [@账号]
    /note add [@账号] 内容
    /note edit [--linked] [@账号] 编号 内容
    /note remove [--linked] [@账号] 编号
    /note clear [--linked] [@账号]
    备注由人手写，按原文保存；bot 可以读到但不会修改，也与它从聊天中学到的内容分开（见 /forget）。成员管理自己账号的备注，owner 可管理任何人的。编号来自同一范围的 /note 或 /who。
what-link = 确认自己的关联账号
detail-link =
    /link @另一个账号
    /link confirm
    /link cancel
    向对方账号发出关联邀请；受邀账号在本群确认，任一方可在本群取消。每个账号在本群同时至多参与一份待确认邀请。
    owner：/link @账号A @账号B 直接关联两个账号，无需邀请。
what-unlink = 解除当前账号关联
detail-unlink =
    /unlink
    只剥离发送指令的账号，其余账号保持关联。
    owner：/unlink @账号 剥离该账号。
what-stats = 查看用量
detail-stats =
    /stats
    /stats global（仅 owner）
what-top = 查看本群使用排行
detail-top =
    /top [--linked] [数量]
    按本月触发回复的次数排行；--linked 按关联账号聚合。
what-tasks = 管理本群定时任务
detail-tasks =
    /tasks [list [页码]]
    /tasks show UUID
    /tasks add (--at 时间 | --in 30m|12h|3d) -- 内容
    /tasks edit UUID [--at 时间 | --in 时长] [-- 内容]
    /tasks cancel UUID
    列出本群待执行与执行中的任务。预约时间不保证准点送达。
what-members = 查看全群成员目录
detail-members = /members
what-block = 管理回复屏蔽
detail-block =
    /block
    /block add [--linked] @账号 [30m|12h|3d]
    /block remove [--linked] @账号
    被屏蔽成员的消息不会触发回复，但仍作为上下文可见。--linked 覆盖该成员当前关联的所有账号。
what-mute = 管理本群静音
detail-mute = /mute [status|on|off]
what-group = 查看 bot 对本群学到的内容
detail-group =
    /group
    从聊天中学到的本群主题和常用说法。owner 可用 /forget group 编号 删除。
what-runs = 查看最近的 bot 运行
detail-runs =
    /runs
    /runs 编号
    列出本群最近的运行，或查看第 N 次运行的步骤：工具调用、结果和发出的消息。
what-logs = 查看最近的警告和错误
detail-logs =
    /logs [条数]
    bot 启动以来最近的警告和错误，最新的在最后。
what-help = 显示指令列表
detail-help = /help [指令名]
who-no_record = 本群暂无该账号资料。
who-title = 账号资料｜{ $display }｜{ $scope }
who-messages = 本群发言：{ $count } 条
who-linked = 关联账号：{ $count } 个
who-names = 已确认称呼：{ $names }
who-leads = 未确认线索（不可用于指认）：{ $names }
name-none = { $display } 暂无记录在案的称呼。
name-header = { $display } 的称呼：
name-confirmed_line = · { $text }（已确认，置信度 { $confidence }）
name-candidate_line = · { $text }（未确认，置信度 { $confidence }）
name-removed = 已撤销 { $display } 的称呼「{ $name }」。
name-remove_none = { $display } 在此范围没有可撤销的称呼「{ $name }」。
name-taken = 「{ $name }」已经指向本群的其他人。
name-empty_name = 称呼不能为空。
name-too_long = 称呼太长，最多 { $max } 个字。
name-added = 已登记：{ $display } 也叫「{ $name }」。
link-confirmed = 已确认关联邀请，双方账号现已关联。
link-cancelled = 已取消本群待处理的关联邀请。
link-nothing_pending = 本群没有与当前账号相关的待处理邀请。
link-invited =
    已向 { $target } 发出关联邀请。
    请受邀账号在本群 { $seconds } 秒内发送：/link confirm
link-self = 不能把账号与自身关联。
link-already = 这两个账号已经关联。
link-busy = 相关账号在本群已有待处理的关联邀请，请先处理该邀请。
link-no_invitation = 本群没有该账号可确认的待处理邀请。
link-expired = 关联邀请已过期，请重新发起。
link-not_target = 只有受邀账号可以确认。
link-out_of_order = 确认消息不在邀请之后，无法确认邀请。
link-stale = 账号关联关系已变化，请重新发起。
link-bot = 不能关联机器人账号。
unlink-not_linked = 这个账号没有和别的账号合并过，不需要拆分。
unlink-done = 已解除当前账号的关联，其余账号保持关联。
link-unknown_account = 账号 { $account } 尚无记录，无法关联。
link-forced = 两个账号现已关联。
link-owner_only = 直接关联两个其他账号需要 bot owner 权限；邀请请用 /link @账号。
unlink-unknown_account = 账号 { $account } 尚无记录，无法剥离。
unlink-forced = 已剥离所选账号，其余账号保持关联。
unlink-owner_only = 剥离其他账号需要 bot owner 权限；/unlink 只剥离你自己的账号。
mute-status_on = 本群已静音。
mute-status_off = 本群未静音。
mute-set_on = 已将本群设为静音。
mute-set_off = 已恢复本群回复。
mute-already_on = 本群已处于静音状态。
mute-already_off = 本群已处于启用回复状态。
block-cannot_block = 不能屏蔽机器人或 bot owner。
block-list_empty = 本群暂无回复屏蔽规则。
block-list_header = 本群回复屏蔽规则：
block-list_row = · { $label }{ $until }
block-until_suffix = （至 { $time }）
block-lapse_until = 至 { $time }
block-lapse_forever = 持续生效
block-scope_exact = 精确账号
block-scope_linked = 关联身份共享
block-removed = 已解除本群回复屏蔽：{ $label }｜{ $scope }。
block-remove_none = 本群没有 { $label } 在{ $scope }范围的屏蔽规则。
block-added = 已设置本群回复屏蔽：{ $label }｜{ $scope }｜{ $lapse }。
members-empty = 本群暂无成员资料。
members-header = 本群 { $total } 人有记录（发言数｜称呼）：
members-row = · { $display }（{ $messages } 条）
members-row_names = · { $display }（{ $messages } 条）｜{ $names }
stats-group_title = 今日用量｜本群 { $group }
stats-global_title = 今日用量｜全部群
stats-runs = 回复：{ $runs } 次（模型 { $model_calls } 次，工具 { $tool_calls } 次）
stats-tokens = 词元：输入 { $input }（缓存 { $cached }），输出 { $output }
stats-replies_muted = 群回复：已静音
stats-replies_enabled = 群回复：已启用
stats-blocks = 屏蔽：{ $count } 条规则
top-header = 本群本月使用排行｜{ $scope }
top-empty = 本群本月暂无可归属账号的使用记录。
top-row = { $rank }. { $name }{ $tag }　{ $runs } 次回复，{ $tokens } 词元
top-linked_tag = （{ $count } 个账号）
tasks-no_mentions = /tasks 只管理本群任务，不接收成员目标。
tasks-bad_id = 任务 ID 或参数格式无效。用法见 /help tasks。
tasks-unknown_action = 未知任务操作。用法见 /help tasks。
tasks-option_value = 时间选项必须包含一个值。
tasks-option_dupe = 只接受一次 --at 或 --in，不接受其它选项。
tasks-at_in_conflict = --at 与 --in 只能选一个。
tasks-bad_time = --at 需要带时区的 RFC 3339 时间，例如 2030-01-02T09:00:00+08:00。
tasks-empty_content = 任务内容不能为空。
tasks-list_empty = 本群暂无待执行或执行中的任务。
tasks-list_page_empty = 此页没有任务，请查看前面的页码。
tasks-list_header = 本群活动任务｜第 { $page } 页（待执行、执行中）
tasks-list_next = 下一页：/tasks list { $page }
tasks-list_footer = 完整内容与结果：/tasks show UUID
tasks-not_found_cancel = 本群没有该任务，或任务已不处于可取消的待执行状态。
tasks-not_found_edit = 本群没有该任务，或任务已不处于可修改的待执行状态。
tasks-not_found = 本群没有该任务。
tasks-shown =
    本群任务详情：
    { $task }
tasks-cancelled =
    已取消本群任务：
    { $task }
tasks-created =
    已创建本群任务：
    { $task }
tasks-edited =
    已修改本群任务：
    { $task }
tasks-detail =
    { $id }
    状态：{ $state }｜预约时间：{ $due }
    { $intent }
tasks-detail_outcome = 执行结果：{ $outcome }
tasks-state_pending = 待执行
tasks-state_running = 执行中
tasks-state_done = 已结束
tasks-state_failed = 执行失败
tasks-state_cancelled = 已取消
tasks-state_interrupted = 已中断
tasks-skipped_muted = 已跳过（本群静音）
tasks-too_soon = 预约时间过近，最早可预约 { $earliest }。
tasks-too_many = 本群待执行任务数量已达上限（{ $limit }）。
tasks-chain_deep = 本群任务续链深度已达上限（{ $limit }）。
tasks-nothing_to_change = 修改任务至少需要内容或新的预约时间。
tasks-empty_intent = 任务内容不能为空。
who-facts = 自动归纳：{ $count } 条
who-no_facts = 自动归纳：暂无
who-fact_line = { $index }. { $predicate ->
        [lives_in] 住在
        [lived_in] 曾住
        [from_place] 来自
        [works_as] 职业
        [works_at] 就职于
        [studies_at] 就读于
        [graduated_from] 毕业于
        [majors_in] 主修
        [birthday] 生日
        [likes] 喜欢
        [dislikes] 不喜欢
        [avoids] 回避
        [wants] 想要
        [fears] 害怕
        [plays] 在玩
        [watches] 在追
        [listens_to] 在听
        [reads] 在读
        [uses] 在用
        [owns] 有
        [collects] 收集
        [has_pet] 养着
        [visited] 去过
        [handle_on] 账号
        [preferred_name] 希望被叫
        [good_at] 擅长
        [speaks] 会说
        [member_of] 属于
        [allergic_to] 过敏
       *[other] { $predicate }
    }：{ $object }（置信度 { $confidence }）
who-notes = 人工备注（{ $count } 条）：
who-no_notes = 人工备注：暂无
who-note_line = { $index }. { $text }
who-hint = 删除备注：/note remove 编号；删除自动归纳：/forget 编号（同一范围）。
note-none = 此范围内暂无关于 { $display } 的备注。
note-header = 关于 { $display } 的备注｜{ $scope }：
note-line = { $index }. { $text }（{ $author }，{ $time }）
note-added = 已添加关于 { $display } 的第 { $index } 条备注。
note-edited = 已修改第 { $index } 条备注。
note-removed = 已删除第 { $index } 条备注：{ $text }
note-cleared = 已删除关于 { $display } 的 { $count } 条备注。
note-no_such = 该范围没有第 { $index } 条备注。发送 /note 查看列表。
note-empty = 备注内容不能为空。
note-too_many = 每个账号最多 { $max } 条备注，请先删除一条。
group-empty = 还没有学到关于本群的内容。
group-header = 对本群学到的内容：
group-topic_line = { $index }. 本群主要聊：{ $text }
group-term_line = { $index }. 「{ $term }」的意思：{ $text }
group-hint = owner 可用 /forget group 编号 删除某一条。
forget-group_done = 已删除第 { $index } 条群知识：{ $text }
forget-group_no_such = 没有第 { $index } 条群知识。发送 /group 查看列表。
runs-empty = 本群还没有运行记录。
runs-header = 本群最近的运行（{ $count } 次）：
runs-row = #{ $id } { $time } { $trigger }｜{ $end }｜{ $turns } 轮，{ $tools } 次工具调用，发出 { $sends } 条｜{ $tokens } 词元
runs-footer = 查看某次运行的步骤：/runs 编号
runs-no_such = 本群没有编号为 { $id } 的运行。
runs-detail = 运行 #{ $id }｜{ $time }｜{ $trigger }｜{ $end }｜{ $tokens } 词元
runs-step_chat = 聊天：{ $count } 行
runs-step_summary = 摘要：{ $text }
runs-step_text = 模型文本：{ $text }
runs-step_call = 调用 { $name }：{ $args }
runs-step_result = 结果（{ $outcome }）：{ $text }
runs-trigger_addressed = 被叫
runs-trigger_wake = 定时
runs-trigger_spontaneous = 主动
runs-open = 仍在运行
logs-empty = 启动以来没有警告或错误。
logs-header = 最近的警告和错误（{ $count } 条）：
logs-line = { $time } { $level } { $text }
usage-forget = 用法：/forget [--linked] [@账号] 编号，或 /forget group 编号
forget-done = 已删除第 { $index } 条：{ $predicate ->
        [lives_in] 住在
        [lived_in] 曾住
        [from_place] 来自
        [works_as] 职业
        [works_at] 就职于
        [studies_at] 就读于
        [graduated_from] 毕业于
        [majors_in] 主修
        [birthday] 生日
        [likes] 喜欢
        [dislikes] 不喜欢
        [avoids] 回避
        [wants] 想要
        [fears] 害怕
        [plays] 在玩
        [watches] 在追
        [listens_to] 在听
        [reads] 在读
        [uses] 在用
        [owns] 有
        [collects] 收集
        [has_pet] 养着
        [visited] 去过
        [handle_on] 账号
        [preferred_name] 希望被叫
        [good_at] 擅长
        [speaks] 会说
        [member_of] 属于
        [allergic_to] 过敏
       *[other] { $predicate }
    }：{ $object }。
forget-no_such = 该范围没有第 { $index } 条自动归纳。发送 /who 查看列表。
what-forget = 删除 bot 从聊天中自动学到的内容
detail-forget =
    /forget [--linked] [@账号] 编号
    /forget group 编号（仅 owner）
    删除 bot 自己学到的内容：/who 中同一范围的第 N 条自动归纳，或 /group 中的第 N 条群知识。不影响人工备注；备注请用 /note 管理。
report-title = 每日报告｜{ $date }
report-runs = 回复：{ $runs } 次（被叫 { $addressed }，定时 { $wake }）
report-ends = 结束方式：{ $ends }
report-errors = 模型错误：{ $errors }
report-count_entry = { $label } { $count }
report-model = 模型调用：{ $calls } 次（失败 { $failed }）
report-tokens = 词元：输入 { $input }（缓存命中 { $hit }%，{ $cached }），输出 { $output }
report-tools = 工具调用：{ $calls } 次（未成功 { $failed }）
report-chat = 聊天：归档 { $messages } 条；在 { $groups } 个群中回复
report-groups = 群：新增 { $new }，静音 { $muted }
report-memory = 记忆：新增 { $episodes } 条经历
report-jobs = 后台任务：待执行 { $pending }，失败 { $failed }；被中断的定时任务 { $interrupted }
report-backup_age = 最近备份：{ $hours } 小时前
report-backup_none = 最近备份：未找到
