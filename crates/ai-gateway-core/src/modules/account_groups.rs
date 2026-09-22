//! 账号分组（按应用分域）：Trae 系与豆包共用同一套结构，各自独立存储。
//!
//! 来源：TraeWorkAssistant `src-tauri/src/commands/accounts.rs` 的分组一节
//! （`groups_list` / `group_create` / `group_update` / `group_delete` / `group_move`），
//! 按本仓库「core 不依赖 Tauri」的约定改写。
//!
//! ## 为什么要按应用分域
//!
//! 上游只有一套全局分组（它当时的账号池基本就是 Trae 系）。本仓库里 Trae 与豆包
//! 是两个完全独立的账号库：同一个 uid 在两边毫无关系，共用一个 membership 表会让
//! 「Trae 的 A 分组」把豆包同名 uid 也一起圈进去 —— 分组数量对不上，用户根本
//! 无法判断哪个分组属于哪个应用。
//!
//! 因此存储形态是 `{ <app>: { groups: [...], membership: {uid: groupId} } }`，
//! 每个应用一份，互不可见。
//!
//! ## 为什么删除分组要连带清 membership
//!
//! 只删 `groups` 不删 `membership` 会留下**孤儿映射**：账号行的 `group_id` 指向一个
//! 不存在的分组，界面按分组筛选时该账号既不在「全部」之外的任何一组里，也不显示
//! 分组名 —— 表现为「账号神秘消失」。删除必须两处一起动。

use serde_json::{json, Value};

use crate::modules::config;

/// 全部支持分组的应用（界面遍历顺序与账号页一致）。
pub const GROUP_APPS: [&str; 2] = ["Trae", "Doubao"];

/// 分组定义。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub color: String,
    pub order: i32,
}

impl Group {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "color": self.color,
            "order": self.order,
        })
    }
}

/// 分组文件路径。
pub fn groups_file() -> std::path::PathBuf {
    config::store_dir().join("account_groups.json")
}

/// 读取整份分组表（从未写过时返回空壳）。
pub fn load_all() -> Value {
    let path = groups_file();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(t.trim_start_matches('\u{feff}')).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

fn save_all(root: &Value) -> Result<(), String> {
    let path = groups_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(root).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| e.to_string())
}

/// 归一化应用名：未知值回退 `Trae`（调用方是自家前端，恒传合法值）。
pub fn normalize_app(app: &str) -> String {
    match app.trim() {
        "Doubao" | "doubao" | "豆包" => "Doubao".to_string(),
        _ => "Trae".to_string(),
    }
}

/// 某应用的分组域（不存在时返回空壳，**不**写盘）。
fn app_scope(root: &Value, app: &str) -> Value {
    root.get(app)
        .cloned()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({ "groups": [], "membership": {} }))
}

/// 某应用的分组列表定义（按 `order` 升序，`order` 相同按 id 稳定排序）。
pub fn list_groups(app: &str) -> Vec<Group> {
    let app = normalize_app(app);
    let root = load_all();
    let scope = app_scope(&root, &app);
    let mut groups: Vec<Group> = scope
        .get("groups")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|g| {
                    Some(Group {
                        id: g.get("id")?.as_str()?.to_string(),
                        name: g.get("name").and_then(Value::as_str).unwrap_or("未命名").to_string(),
                        color: g.get("color").and_then(Value::as_str).unwrap_or("slate").to_string(),
                        order: g.get("order").and_then(Value::as_i64).unwrap_or(0) as i32,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    groups.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.id.cmp(&b.id)));
    groups
}

/// uid → groupId 映射（仅返回仍然存在的分组，孤儿映射被过滤掉）。
///
/// 过滤孤儿而不是原样返回：历史数据（分组被手工改过文件、或旧版本删组没清映射）
/// 会留下指向不存在分组的记录，界面若照单全收就会显示一个点不开的分组标签。
pub fn membership(app: &str) -> serde_json::Map<String, Value> {
    let app = normalize_app(app);
    let root = load_all();
    let scope = app_scope(&root, &app);
    let valid: std::collections::HashSet<String> =
        list_groups(&app).into_iter().map(|g| g.id).collect();
    scope
        .get("membership")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter(|(_, v)| {
                    v.as_str()
                        .map(|g| valid.contains(g))
                        .unwrap_or(false)
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// 分组视图：定义 + 成员 uid 列表 + 计数。
pub fn list_view(app: &str) -> Value {
    let app = normalize_app(app);
    let member = membership(&app);
    let groups: Vec<Value> = list_groups(&app)
        .iter()
        .map(|g| {
            let uids: Vec<String> = member
                .iter()
                .filter(|(_, v)| v.as_str() == Some(g.id.as_str()))
                .map(|(k, _)| k.clone())
                .collect();
            let mut view = g.to_json();
            view["count"] = json!(uids.len());
            view["uids"] = json!(uids);
            view
        })
        .collect();
    json!({ "app": app, "groups": groups, "membership": member })
}

/// 写回某应用的分组域。
fn write_scope(app: &str, scope: Value) -> Result<(), String> {
    let app = normalize_app(app);
    let mut root = load_all();
    if !root.is_object() {
        root = json!({});
    }
    root[app] = scope;
    save_all(&root)
}

/// 新建分组，返回新分组 id。
pub fn create_group(app: &str, name: &str, color: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("分组名不能为空".to_string());
    }
    let app = normalize_app(app);
    if list_groups(&app).iter().any(|g| g.name == name) {
        return Err(format!("分组「{name}」已存在"));
    }
    let root = load_all();
    let mut scope = app_scope(&root, &app);
    let groups = scope
        .get_mut("groups")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "分组表结构异常".to_string())?;
    let order = groups.len() as i32 + 1;
    // id 用毫秒时间戳 + 组名哈希，避免同一毫秒内连续建两个组时撞 id
    let id = format!(
        "g_{}",
        chrono::Utc::now().timestamp_millis()
    );
    let id = if groups
        .iter()
        .any(|g| g.get("id").and_then(Value::as_str) == Some(id.as_str()))
    {
        format!("{id}_{}", groups.len())
    } else {
        id
    };
    groups.push(json!({
        "id": id,
        "name": name,
        "color": if color.trim().is_empty() { "slate" } else { color.trim() },
        "order": order,
    }));
    write_scope(&app, scope)?;
    Ok(id)
}

/// 更新分组（只改传入的字段）。
pub fn update_group(
    app: &str,
    id: &str,
    name: Option<&str>,
    color: Option<&str>,
    order: Option<i32>,
) -> Result<(), String> {
    let app = normalize_app(app);
    let root = load_all();
    let mut scope = app_scope(&root, &app);
    let groups = scope
        .get_mut("groups")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "分组表结构异常".to_string())?;
    let target = groups
        .iter_mut()
        .find(|g| g.get("id").and_then(Value::as_str) == Some(id))
        .ok_or_else(|| format!("分组 {id} 不存在"))?;
    let obj = target
        .as_object_mut()
        .ok_or_else(|| "分组记录格式异常".to_string())?;
    if let Some(n) = name.map(str::trim).filter(|s| !s.is_empty()) {
        obj.insert("name".to_string(), json!(n));
    }
    if let Some(c) = color.map(str::trim).filter(|s| !s.is_empty()) {
        obj.insert("color".to_string(), json!(c));
    }
    if let Some(o) = order {
        obj.insert("order".to_string(), json!(o));
    }
    write_scope(&app, scope)
}

/// 删除分组：**同时**清掉该分组的全部成员映射（见模块头「为什么」）。
pub fn delete_group(app: &str, id: &str) -> Result<(), String> {
    let app = normalize_app(app);
    let root = load_all();
    let mut scope = app_scope(&root, &app);
    let groups = scope
        .get_mut("groups")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "分组表结构异常".to_string())?;
    let before = groups.len();
    groups.retain(|g| g.get("id").and_then(Value::as_str) != Some(id));
    if groups.len() == before {
        return Err(format!("分组 {id} 不存在"));
    }
    if let Some(m) = scope.get_mut("membership").and_then(Value::as_object_mut) {
        m.retain(|_, v| v.as_str() != Some(id));
    }
    write_scope(&app, scope)
}

/// 把账号移入分组（`group_id = None` 表示移出分组）。
pub fn move_account(app: &str, user_id: &str, group_id: Option<&str>) -> Result<(), String> {
    let user_id = user_id.trim();
    if user_id.is_empty() {
        return Err("缺少账号标识（userId）".to_string());
    }
    let app = normalize_app(app);
    if let Some(gid) = group_id.map(str::trim).filter(|s| !s.is_empty()) {
        if !list_groups(&app).iter().any(|g| g.id == gid) {
            return Err(format!("分组 {gid} 不存在，无法把账号移入"));
        }
    }
    let root = load_all();
    let mut scope = app_scope(&root, &app);
    if !scope.get("membership").map(Value::is_object).unwrap_or(false) {
        scope["membership"] = json!({});
    }
    let m = scope
        .get_mut("membership")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| "分组成员表结构异常".to_string())?;
    match group_id.map(str::trim).filter(|s| !s.is_empty()) {
        Some(gid) => {
            m.insert(user_id.to_string(), json!(gid));
        }
        None => {
            m.remove(user_id);
        }
    }
    write_scope(&app, scope)
}

/// 批量删除分组内的全部账号映射（删除账号时调用，避免残留映射）。
pub fn forget_account(app: &str, user_id: &str) {
    let app = normalize_app(app);
    let root = load_all();
    let mut scope = app_scope(&root, &app);
    let Some(m) = scope.get_mut("membership").and_then(Value::as_object_mut) else {
        return;
    };
    if m.remove(user_id.trim()).is_some() {
        let _ = write_scope(&app, scope);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个用例独占一个数据目录。
    ///
    /// 必须用 `Isolated` 而不是自建互斥锁：`Isolated` 通过进程级环境变量
    /// `AI_GATEWAY_HOME` 切换数据目录，其它模块的用例也在改它 ——
    /// 自建锁只能串行化本模块的用例，挡不住别的模块在中间把目录换掉，
    /// 表现为「刚建好的组突然不存在」。`Isolated` 内部用的是全局锁，
    /// 顺带把跨模块的交错也排除了。
    use crate::modules::config::test_isolation::Isolated;

    fn reset(app: &str) {
        let root = json!({});
        let _ = write_scope(app, app_scope(&root, app));
    }

    #[test]
    fn 应用名归一化() {
        assert_eq!(normalize_app("Doubao"), "Doubao");
        assert_eq!(normalize_app("doubao"), "Doubao");
        assert_eq!(normalize_app("豆包"), "Doubao");
        assert_eq!(normalize_app("Trae"), "Trae");
        assert_eq!(normalize_app("TraeWork"), "Trae");
        assert_eq!(normalize_app("nonsense"), "Trae");
    }

    #[test]
    fn 建组_改名_删组_全流程() {
        let _iso = Isolated::new("groups");
        let app = "Trae".to_string();
        reset(&app);
        let id = create_group(&app, "测试组A", "blue").unwrap();
        assert!(list_groups(&app).iter().any(|g| g.id == id));

        update_group(&app, &id, Some("测试组B"), Some("red"), Some(9)).unwrap();
        let g = list_groups(&app).into_iter().find(|g| g.id == id).unwrap();
        assert_eq!(g.name, "测试组B");
        assert_eq!(g.color, "red");
        assert_eq!(g.order, 9);

        delete_group(&app, &id).unwrap();
        assert!(!list_groups(&app).iter().any(|g| g.id == id));
        reset(&app);
    }

    #[test]
    fn 同名分组被拒绝() {
        let _iso = Isolated::new("groups");
        let app = "Trae".to_string();
        reset(&app);
        create_group(&app, "重名组", "slate").unwrap();
        assert!(create_group(&app, "重名组", "slate").is_err());
        reset(&app);
    }

    #[test]
    fn 空名分组被拒绝() {
        let _iso = Isolated::new("groups");
        let app = "Trae".to_string();
        reset(&app);
        assert!(create_group(&app, "   ", "slate").is_err());
    }

    #[test]
    fn 移入移出分组() {
        let _iso = Isolated::new("groups");
        let app = "Trae".to_string();
        reset(&app);
        let id = create_group(&app, "成员组", "green").unwrap();
        move_account(&app, "u-1", Some(&id)).unwrap();
        assert_eq!(
            membership(&app).get("u-1").and_then(Value::as_str),
            Some(id.as_str())
        );
        move_account(&app, "u-1", None).unwrap();
        assert!(membership(&app).get("u-1").is_none());
        reset(&app);
    }

    /// 关键回归：删组必须连带清 membership，否则账号会「神秘消失」。
    #[test]
    fn 删组连带清成员映射避免孤儿() {
        let _iso = Isolated::new("groups");
        let app = "Trae".to_string();
        reset(&app);
        let id = create_group(&app, "待删组", "amber").unwrap();
        move_account(&app, "u-orphan", Some(&id)).unwrap();
        delete_group(&app, &id).unwrap();
        assert!(
            membership(&app).get("u-orphan").is_none(),
            "删组后不得留下指向不存在分组的孤儿映射"
        );
        reset(&app);
    }

    #[test]
    fn 移入不存在的分组被拒绝() {
        let _iso = Isolated::new("groups");
        let app = "Trae".to_string();
        reset(&app);
        let err = move_account(&app, "u-2", Some("g_not_exist")).unwrap_err();
        assert!(err.contains("不存在"), "{err}");
    }

    #[test]
    fn 孤儿映射在读取时被过滤() {
        let _iso = Isolated::new("groups");
        let app = "Trae".to_string();
        reset(&app);
        // 直接写一份带孤儿映射的表（模拟手工改文件 / 旧版本残留）
        let _ = write_scope(
            &app,
            json!({
                "groups": [{"id":"g_live","name":"活组","color":"slate","order":1}],
                "membership": {"u-live":"g_live", "u-dead":"g_gone"}
            }),
        );
        let m = membership(&app);
        assert_eq!(m.get("u-live").and_then(Value::as_str), Some("g_live"));
        assert!(
            m.get("u-dead").is_none(),
            "指向不存在分组的映射必须被过滤"
        );
        reset(&app);
    }

    #[test]
    fn 分组视图带成员与计数() {
        let _iso = Isolated::new("groups");
        let app = "Trae".to_string();
        reset(&app);
        let id = create_group(&app, "视图组", "violet").unwrap();
        move_account(&app, "u-a", Some(&id)).unwrap();
        move_account(&app, "u-b", Some(&id)).unwrap();
        let view = list_view(&app);
        let g = view["groups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["id"] == json!(id))
            .unwrap();
        assert_eq!(g["count"], json!(2));
        assert_eq!(g["uids"].as_array().unwrap().len(), 2);
        assert_eq!(view["app"], json!(app));
        reset(&app);
    }

    #[test]
    fn 两个应用的分组互相隔离() {
        reset("Trae");
        reset("Doubao");
        let t = create_group("Trae", "Trae 专用组", "blue").unwrap();
        move_account("Trae", "same-uid", Some(&t)).unwrap();

        assert!(
            list_groups("Doubao").is_empty(),
            "豆包不应看到 Trae 的分组"
        );
        assert!(
            membership("Doubao").get("same-uid").is_none(),
            "同名 uid 在两个应用里毫无关系，不得串台"
        );
        reset("Trae");
        reset("Doubao");
    }

    #[test]
    fn 账号被删除时映射一并忘掉() {
        let _iso = Isolated::new("groups");
        let app = "Trae".to_string();
        reset(&app);
        let id = create_group(&app, "遗忘组", "slate").unwrap();
        move_account(&app, "u-forget", Some(&id)).unwrap();
        forget_account(&app, "u-forget");
        assert!(membership(&app).get("u-forget").is_none());
        // 分组本身仍在
        assert!(list_groups(&app).iter().any(|g| g.id == id));
        reset(&app);
    }

    #[test]
    fn 更新不存在的分组报错() {
        let _iso = Isolated::new("groups");
        let app = "Trae".to_string();
        reset(&app);
        assert!(update_group(&app, "g_nope", Some("x"), None, None).is_err());
        assert!(delete_group(&app, "g_nope").is_err());
    }
}
