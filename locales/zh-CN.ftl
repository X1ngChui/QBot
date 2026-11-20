# Simplified Chinese catalog (Fluent). Same message ids and variables as en.ftl.
writing-language = Simplified Chinese
command-owner_only = 此操作仅限机器人所有者。
command-delegated_only = 此操作仅限机器人所有者或本群的授权管理员。
command-delegated_target = 授权管理员只能操作本群的普通成员，不能操作机器人所有者或其他授权管理员。
command-authority_unknown = 暂时无法确认你的权限，未做任何更改，请稍后重试。
command-outcome_unknown = 无法确认操作结果。请先确认是否已生效，再决定是否重试。
command-unknown_account = 该账号暂无记录。
command-mentions_exact = 请 @ 恰好 { $count } 个账号。
command-mentions_max = 最多 @ { $max } 个账号。
command-bad_number = 编号和页码须为 1 到 10000 的整数。用法见 /help。
command-linked_once = --linked 只能使用一次。
command-unknown_option = 未知选项。用法见 /help。
command-own_accounts_only = 只能操作自己的账号及其关联账号。
command-duration_invalid = 时长格式无效，请使用 30m、12h、3d 这样的写法。
command-truncated = （已截断）
command-list_separator = 、
command-display_fallback = 账号 { $account }
command-display_member = 成员 { $number }
command-scope_exact = 单个账号
command-scope_linked = 关联账号
usage-runs = 用法：/runs [运行编号]
usage-logs = 用法：/logs [条数]
usage-link_confirm = 用法：/link confirm
usage-link_cancel = 用法：/link cancel
usage-link_issue = 用法：/link @账号、/link confirm 或 /link cancel
usage-unlink = 用法：/unlink
usage-stats = 用法：/stats [global]
usage-top = 用法：/top [--linked] [数量]
usage-tasks_list = 用法：/tasks list [页码]
usage-tasks_one = 用法：/tasks { $action } ID
usage-tasks_add = 用法：/tasks add (--at 时间 | --in 时长 | --every 规则 [--at 时间 | --in 时长]) -- 内容
usage-tasks_edit = 用法：/tasks edit ID [--at 时间 | --in 时长] [--every 规则] [-- 内容]
usage-members = 用法：/members
usage-block = 用法：/block add|remove [--linked] @账号 [时长]
usage-block_add = 用法：/block add [--linked] @账号 [30m|12h|3d]
usage-block_remove = 用法：/block remove [--linked] @账号
usage-mute = 用法：/mute [status|on|off]
usage-help = 用法：/help [指令名]
help-header = 可用指令：
help-category_profile = 记忆
help-category_identity = 身份
help-category_group = 本群
help-category_admin = 管理
help-category_help = 帮助
help-entry = { $command }　{ $what }
help-owner_tag = （仅所有者）
help-delegated_tag = （所有者或授权管理员）
help-footer = 详细用法：/help 指令名
help-unknown = 没有「{ $name }」这条指令。发送 /help 查看指令列表。
help-access_member = 权限：所有成员
help-access_owner = 权限：仅机器人所有者
help-access_delegated = 权限：机器人所有者和本群授权管理员
help-detail =
    { $command }　{ $what }
    { $access }

    { $detail }
what-admin = 管理本群的授权管理员
detail-admin =
    /admin
    /admin add @账号
    /admin remove @账号

    授权管理员仅在本群拥有以下权限：
    - 使用 /mute、/block 和 /members。
    - 管理对普通成员和本群的了解（/memory）。

    限制：
    - 不能操作机器人所有者或其他授权管理员，也不能改动机器人所有者添加的内容或设置的屏蔽。
    - 只有机器人所有者可以添加或移除授权管理员。
    - 权限只属于该账号本身，不延伸到它的关联账号。
usage-admin = 用法：/admin、/admin add @账号 或 /admin remove @账号
admin-list_empty = 本群没有授权管理员。
admin-list_header = 本群的授权管理员：
admin-list_row = - { $label }
admin-added = 已将 { $label } 设为本群授权管理员。权限范围见 /help admin。
admin-already = { $label } 已是本群授权管理员。
admin-removed = { $label } 不再是本群授权管理员。
admin-remove_none = { $label } 不是本群授权管理员。
admin-bot = 机器人不能成为授权管理员。
admin-bot_owner = 机器人所有者已拥有全部权限。
what-link = 关联自己的账号
detail-link =
    /link @账号
    /link confirm
    /link cancel

    /link @账号 向对方发出关联邀请；受邀账号在本群发送 /link confirm 确认，任一方可用 /link cancel 取消。
    - 每个账号在每个群同时只能有一份待确认的邀请。
    - 关联只在建立它的群内有效。

    机器人所有者：/link @账号A @账号B 可直接关联两个账号，无需邀请。
what-unlink = 解除自己账号的关联
detail-unlink =
    /unlink

    仅在本群解除发送指令的账号的关联，其余账号保持关联。

    机器人所有者：/unlink @账号 可解除该账号的关联。
what-stats = 查看今日用量
detail-stats =
    /stats
    /stats global（仅所有者）

    /stats 显示本群今日用量；/stats global 显示所有群的今日用量。
what-top = 查看本月使用排行
detail-top =
    /top [--linked] [数量]

    按本月发起回复的次数排行。--linked 将关联账号合并为一人统计。
what-tasks = 管理本群的定时任务
detail-tasks =
    /tasks [list [页码]]
    /tasks show ID
    /tasks add (--at 时间 | --in 30m|12h|3d) -- 内容
    /tasks add --every 规则 [--at 时间 | --in 时长] -- 内容
    /tasks edit ID [--at 时间 | --in 时长] [--every 规则] [-- 内容]
    /tasks cancel ID

    列出本群待执行和执行中的任务。任务的实际执行时间可能晚于预约时间。

    规则让任务重复执行，直到被取消，按机器人的时区计算：
    - 固定间隔：2h、3d
    - 每天：daily@08:00
    - 指定星期：mon,thu@20:30
    使用规则时，--at 或 --in 指定最早的首次执行时间。机器人离线期间错过的多次执行只补一次。
what-members = 查看本群成员列表
detail-members =
    /members

    列出本群有记录的全部成员，以及各自的发言数和称呼。
what-block = 管理回复屏蔽
detail-block =
    /block
    /block add [--linked] @账号 [30m|12h|3d]
    /block remove [--linked] @账号

    /block 列出当前的屏蔽。被屏蔽成员的消息不会触发回复，但机器人仍会把它们当作上下文阅读。不写时长则一直有效，直到解除。--linked 涵盖该成员当前关联的所有账号。
what-mute = 设置本群静音
detail-mute =
    /mute [status|on|off]

    静音期间机器人不在本群回复。只发送 /mute 可查看当前状态。
what-runs = 查看最近的运行记录
detail-runs =
    /runs
    /runs 编号

    /runs 列出本群最近的运行；/runs 编号 显示该次运行的步骤：工具调用、调用结果和发出的消息。
what-logs = 查看最近的警告和错误
detail-logs =
    /logs [条数]

    显示机器人启动以来最近的警告和错误，最新的排在最后。与本群有关的条目完整显示；不属于任何群的条目不显示细节。
what-help = 查看指令列表或某条指令的用法
detail-help =
    /help [指令名]

    /help 列出你可以使用的指令；/help 指令名 说明该指令的用法。
who-no_record = 本群暂无该账号的记录。
link-confirmed = 关联已确认，两个账号现已关联。
link-cancelled = 已取消本群待确认的关联邀请。
link-nothing_pending = 当前账号在本群没有待确认的关联邀请。
link-invited =
    已向 { $target } 发出关联邀请。
    请受邀账号在 { $seconds } 秒内于本群发送：/link confirm
link-self = 账号不能与自身关联。
link-already = 这两个账号在本群已经关联。
link-busy = 其中一个账号在本群已有待确认的邀请，请先确认或取消。
link-no_invitation = 当前账号在本群没有可确认的邀请。
link-expired = 邀请已过期，请重新发起。
link-not_target = 只有受邀账号可以确认邀请。
link-stale = 发出邀请后，相关账号的关联关系已变化，请重新发起。
link-bot = 机器人自己的账号不能关联。
unlink-not_linked = 当前账号在本群没有关联其他账号。
unlink-done = 已解除当前账号的关联，其余账号保持关联。
link-unknown_account = 账号 { $account } 暂无记录，无法关联。
link-forced = 两个账号现已关联。
link-owner_only = 只有机器人所有者可以直接关联其他两个账号。请用 /link @账号 发出邀请。
unlink-unknown_account = 账号 { $account } 暂无记录，无法解除关联。
unlink-forced = 已解除所选账号的关联，其余账号保持关联。
unlink-owner_only = 只有机器人所有者可以解除其他账号的关联。/unlink 只解除你自己的账号。
mute-status_on = 本群已静音。
mute-status_off = 本群未静音。
mute-set_on = 已将本群静音。
mute-set_off = 已恢复本群回复。
mute-already_on = 本群已经是静音状态。
mute-already_off = 本群回复本来就是开启的。
block-cannot_block = 不能屏蔽机器人或机器人所有者。
block-delegated_target = 授权管理员只能屏蔽普通成员，不能屏蔽其他授权管理员。
block-owner_imposed = { $label } 的屏蔽由机器人所有者设置，只有机器人所有者可以修改或解除。
block-list_empty = 本群没有回复屏蔽。
block-list_header = 本群的回复屏蔽：
block-list_row = - { $label }{ $until }
block-until_suffix = （至 { $time }）
block-lapse_until = 至 { $time }
block-lapse_forever = 直到解除
block-scope_exact = 单个账号
block-scope_linked = 全部关联账号
block-removed = 已解除回复屏蔽：{ $label }｜{ $scope }。
block-remove_none = { $label } 没有回复屏蔽（{ $scope }）。
block-added = 已屏蔽回复：{ $label }｜{ $scope }｜{ $lapse }。
members-empty = 本群暂无成员记录。
members-header = 本群有记录的成员共 { $total } 人（发言数｜称呼）：
members-row = - { $display }（{ $messages } 条）
members-row_names = - { $display }（{ $messages } 条）｜{ $names }
stats-group_title = 今日用量｜本群 { $group }
stats-global_title = 今日用量｜全部群
stats-runs = 回复：{ $runs } 次（工具调用 { $tool_calls } 次）
stats-replies_muted = 群回复：已静音
stats-replies_enabled = 群回复：已开启
stats-blocks = 回复屏蔽：{ $count } 条
top-header = 本月使用排行｜{ $scope }
top-empty = 本月本群没有可归属到账号的回复记录。
top-row = { $rank }. { $name }{ $tag }　{ $runs } 次回复，{ $tokens } Token
top-linked_tag = （{ $count } 个账号）
tasks-no_mentions = /tasks 只管理本群任务，不接受 @ 成员。
tasks-bad_id = 任务 ID 或参数无效。用法见 /help tasks。
tasks-unknown_action = 未知的任务操作。用法见 /help tasks。
tasks-option_value = 每个时间选项必须且只能带一个值。
tasks-option_dupe = --at、--in、--every 各只能使用一次，且不接受其他选项。
tasks-bad_every = 规则无效。--every 可写间隔（2h、3d）、daily@时:分，或星期加时间（如 mon,thu@20:30）。
tasks-interval_too_short = 重复任务的间隔不能短于 { $min }。
tasks-at_in_conflict = --at 和 --in 只能二选一。
tasks-bad_time = --at 需要带时区偏移的 RFC 3339 时间，例如 2030-01-02T09:00:00+08:00。
tasks-empty_content = 任务内容不能为空。
tasks-list_empty = 本群没有待执行或执行中的任务。
tasks-list_page_empty = 这一页没有任务，请查看更前面的页。
tasks-list_header = 本群进行中的任务｜第 { $page } 页（待执行和执行中）
tasks-list_next = 下一页：/tasks list { $page }
tasks-list_footer = 查看完整内容和结果：/tasks show ID
tasks-not_found_cancel = 本群没有这个任务，或它已不是待执行状态，无法取消。
tasks-not_found_edit = 本群没有这个任务，或它已不是待执行状态，无法修改。
tasks-not_found = 本群没有这个任务。
tasks-shown =
    任务详情：
    { $task }
tasks-cancelled =
    已取消任务：
    { $task }
tasks-cancelled_repeating =
    已取消重复任务，之后不再执行；正在进行的这一次会执行完。
    { $task }
tasks-created =
    已创建任务：
    { $task }
tasks-edited =
    已更新任务：
    { $task }
tasks-detail =
    { $id }
    状态：{ $state }｜预约时间：{ $due }
    { $intent }
tasks-detail_outcome = 执行结果：{ $outcome }
tasks-detail_repeat = 重复：{ $rule }。预约时间为下一次执行的时间。
tasks-detail_last_run = 上次执行（预约 { $due }）：{ $outcome }
tasks-repeat_interval = 每 { $interval }
tasks-repeat_daily = 每天 { $time }（{ $zone }）
tasks-repeat_weekly = 每{ $days } { $time }（{ $zone }）
tasks-weekday = { $day ->
        [mon] 周一
        [tue] 周二
        [wed] 周三
        [thu] 周四
        [fri] 周五
        [sat] 周六
       *[sun] 周日
    }
tasks-state_pending = 待执行
tasks-state_running = 执行中
tasks-state_done = 已完成
tasks-state_failed = 失败
tasks-state_cancelled = 已取消
tasks-state_interrupted = 已中断
tasks-skipped_muted = 已跳过（当时本群静音）
tasks-too_soon = 预约时间太近，最早可预约 { $earliest }。
tasks-too_many = 本群待执行任务已达上限（{ $limit } 个）。
tasks-chain_deep = 后续任务的连续层数已达上限（{ $limit } 层）。
tasks-nothing_to_change = 没有可修改的内容。请提供新的内容、时间或规则。
tasks-empty_intent = 任务内容不能为空。
runs-empty = 本群暂无运行记录。
runs-header = 本群最近的运行（{ $count } 次）：
runs-row = #{ $id } { $time } { $trigger }｜{ $end }｜{ $turns } 轮，工具调用 { $tools } 次，发出 { $sends } 条｜{ $tokens } Token
runs-footer = 查看某次运行的步骤：/runs 编号
runs-no_such = 本群没有编号为 #{ $id } 的运行。
runs-detail = 运行 #{ $id }｜{ $time }｜{ $trigger }｜{ $end }｜{ $tokens } Token
runs-step_chat = 聊天：{ $count } 行
runs-step_summary = 摘要：{ $text }
runs-step_trim = 超出模型上下文：省略了较早的 { $count } 行
runs-step_text = 模型文本：{ $text }
runs-step_call = 调用 { $name }：{ $args }
runs-step_result = 结果（{ $outcome }）：{ $text }
runs-trigger_addressed = 被呼叫
runs-trigger_wake = 定时任务
runs-trigger_spontaneous = 主动发言
runs-open = 仍在运行
logs-empty = 机器人启动以来没有警告或错误。
logs-header = 最近的警告和错误（{ $count } 条）：
logs-line = { $time } { $level } { $text }
report-title = 每日报告｜{ $date }
report-runs = 回复：{ $runs } 次（被呼叫 { $addressed }，定时任务 { $wake }，主动发言 { $spontaneous }）
report-ends = 运行结果：{ $ends }
report-errors = 模型错误：{ $errors }
report-count_entry = { $label } { $count }
calls-none = 服务调用：无
calls-line = - { $purpose }·{ $kind }：{ $calls } 次（请求 { $requests }，失败 { $failed }）；{ $amounts }
calls-total = 服务调用合计：{ $calls } 次（请求 { $requests }，失败 { $failed }）；输入 { $input } Token，输出 { $output } Token；额度 { $credits }
calls-tokens = 输入 { $input } Token（{ $cache }），输出 { $output } Token
calls-cached = 缓存 { $cached }，{ $hit }%
calls-cache_unreported = 未报告缓存
calls-input_tokens = { $input } Token
calls-credits = 额度 { $credits }
calls-credits_unreported = 未报告额度
purpose-reply = 回复
purpose-extraction = 记忆
purpose-picture = 图片
purpose-unattributed = 未归属
call_kind-model = 模型
call_kind-embedding = 向量
call_kind-search = 网页搜索
call_kind-extract = 网页阅读
report-tools = 工具调用：{ $calls } 次（未成功 { $failed }）
report-chat = 聊天：归档 { $messages } 条消息；在 { $groups } 个群中回复
report-groups = 群：新增 { $new }，静音 { $muted }
report-memory = 记忆：新增 { $episodes } 段经历
report-jobs = 后台任务：待执行 { $pending }，失败 { $failed }；被中断的定时任务 { $interrupted }
report-backup_age = 最近备份：{ $hours } 小时前
report-backup_none = 最近备份：未找到

memory-replaced_line = { $id } { $text }（{ $date } 已被新的说法取代）
memory-ended_line = { $id } { $text }（{ $date } 起不再成立）
memory-line = { $id } { $text }（{ $source }，见于 { $count } 段对话，最近一次 { $date }）
memory-source_own = 本人说的
memory-source_others = 别人说的
memory-source_both = 本人和别人都说过
memory-source_members = 群友说的
usage-memory = 用法：/memory [@账号 | group]，或 /memory add | name | fix | end | forget ……（详见 /help memory）
what-memory = 查看和管理机器人对成员或本群的记忆
detail-memory =
    /memory [@账号]：查看关于某位成员的记忆（称呼、内容和已不成立的条目）
    /memory group：查看关于本群的记忆
    /memory add [@账号 | group] 内容：手动记下一条
    /memory name [@账号] 称呼：记下该成员的一个称呼
    /memory fix 编号 内容：更正一条，原条目留档备查
    /memory end 编号：标记为不再成立
    /memory forget 编号：忘掉一条，以后聊天里再提到也不会再记住
    /memory forget all [@账号]：忘掉关于某位成员的全部内容

    机器人每天从聊天中整理一次记忆；手动添加的内容立即生效，聊天不会覆盖它。编号形如 m12。

    谁可以修改：
    - 成员：关于自己（及关联账号）的内容。
    - 授权管理员：另外还有本群的普通成员和本群本身，但不能改动机器人所有者为他人添加的内容。
    - 机器人所有者：全部内容。
memory-title = 关于 { $display } 的记忆
memory-group_title = 关于本群的记忆
memory-messages = 在本群发过 { $count } 条消息
memory-linked = 已关联 { $count } 个账号
memory-names = 称呼（{ $count } 个）：
memory-name_line = { $id } { $text }（{ $how }）
memory-name_hand = 手动添加
memory-name_own = 本人在 { $count } 段对话中这样自称
memory-name_others = 别人在 { $count } 段对话中这样叫，最近一次 { $date }
memory-how_separator = ；
memory-known = 记得的内容（{ $count } 条）：
memory-nothing = 记得的内容：暂无
memory-hand_line = { $id } { $text }（{ $by } 于 { $date } 手动添加）
memory-history = 已不成立，留档备查（{ $count } 条）：
memory-hint = 要修改某条，请在 /memory fix、end 或 forget 后加上编号，例如 /memory forget m12。详见 /help memory
memory-added = 已记下关于 { $display } 的这条内容，编号 { $id }。
memory-added_group = 已记下关于本群的这条内容，编号 { $id }。
memory-name_added = 已记下：{ $display } 也叫「{ $name }」（{ $id }）。
memory-known_already = 这条已经记过了，编号 { $id }。
memory-empty = 内容不能为空。
memory-too_long = 每条最多 { $max } 个字符，一句话就够了。
memory-name_too_long = 称呼最多 { $max } 个字符。
memory-fixed = 已更正为 { $new } { $text }。原来的 { $old } 留档备查。
memory-ended = 已标记为不再成立：{ $id } { $text }
memory-forgot = 已忘掉 { $id }：{ $text }。以后聊天里再提到，也不会再记住。
memory-forgot_all =
    { $count ->
        [0] 关于 { $display } 还没有记下任何内容。
       *[other] 已忘掉关于 { $display } 的 { $count } 条内容。以后聊天里再提到，也不会再记住。
    }
memory-no_such = 找不到你能修改的 { $id }。发送 /memory 查看编号。
memory-not_current = { $id } 已经不成立了，只能用 forget 删除。
memory-owner_entered = 这条是机器人所有者添加的，只有机器人所有者或当事人本人可以修改。
memory-group_admins = 只有机器人所有者和授权管理员可以修改本群的记忆。
memory-too_many = 每人最多手动添加 { $max } 条，请先删掉一条。
