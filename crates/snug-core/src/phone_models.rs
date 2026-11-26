//! 机型库，移植自 C# v1.3.0 `SnugWin.Core.PhoneScreen.PhoneModels`。
//!
//! 数据源是 `assets/phone-models.json`：除了老的 `{ "brands": [...] }` 结构，
//! 现在还支持 v1.3 的 `{ "phone": [...], "tablet": [...], "computer": [...] }`
//! 三段式（上游 `device-models.json` 的写法），每段里的机型自动带上设备种类。
//! 竖握书写：短边在前。校验规则与 C# 版一致，出错时给出中文原因。

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::geometry::Size;
use crate::screen_fit::{PhoneSpec, ScreenFit};

/// 虚拟手机屏的显示名（C# `PhoneScreenSpec.Name`）。
pub const PHONE_SCREEN_NAME: &str = "Snug 手机屏";

/// 设备种类，对应 C# `DeviceKind`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceKind {
    #[default]
    Phone,
    Tablet,
    Computer,
}

impl DeviceKind {
    /// 触屏设备（手机、平板）；电脑屏不接管触摸。
    pub fn is_touch(self) -> bool {
        matches!(self, Self::Phone | Self::Tablet)
    }

    /// 这个种类最常见的握持方向：手机竖屏，其余横屏。
    pub fn usual_orientation(self) -> ScreenOrientation {
        match self {
            Self::Phone => ScreenOrientation::Portrait,
            _ => ScreenOrientation::Landscape,
        }
    }

    /// 这个种类允许的方向（电脑只能横屏）。
    pub fn allowed_orientations(self) -> &'static [ScreenOrientation] {
        match self {
            Self::Computer => &[ScreenOrientation::Landscape],
            _ => &[ScreenOrientation::Portrait, ScreenOrientation::Landscape],
        }
    }

    /// 能互相「换种类」的集合：手机↔平板（都是触屏），电脑只能和电脑互换。
    pub fn swappable_kinds(self) -> &'static [DeviceKind] {
        if self.is_touch() {
            &[DeviceKind::Phone, DeviceKind::Tablet]
        } else {
            &[DeviceKind::Computer]
        }
    }

    /// 界面上的中文名（C# `DisplayName()`）。
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Phone => "手机",
            Self::Tablet => "平板",
            Self::Computer => "电脑",
        }
    }

    /// C# 枚举名，只用于写日志/错误文案（C# 里 `$"{kind}"` 就是这几个词）。
    pub fn label(self) -> &'static str {
        match self {
            Self::Phone => "Phone",
            Self::Tablet => "Tablet",
            Self::Computer => "Computer",
        }
    }
}

/// 屏幕方向，对应 C# `ScreenOrientation`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScreenOrientation {
    #[default]
    Portrait,
    Landscape,
}

impl ScreenOrientation {
    /// 界面上的中文名（C# `DisplayName()`）。
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Portrait => "竖屏",
            Self::Landscape => "横屏",
        }
    }

    /// 与 C# 枚举同名，写配置/日志时用。
    pub fn label(self) -> &'static str {
        match self {
            Self::Portrait => "Portrait",
            Self::Landscape => "Landscape",
        }
    }
}

/// 机型条目里的屏幕规格。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhoneScreenSpec {
    pub width: i32,
    pub height: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub points: Option<i32>,
    /// 挖孔/刘海占掉的像素（电脑屏横屏时从宽度里扣掉）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notch: Option<i32>,
}

impl PhoneScreenSpec {
    pub const fn new(width: i32, height: i32, points: Option<i32>) -> Self {
        Self { width, height, points, notch: None }
    }

    /// 带挖孔宽度的规格（C# `PhoneScreenSpecData(width, height, points, notch)`）。
    pub const fn with_notch(mut self, notch: Option<i32>) -> Self {
        self.notch = notch;
        self
    }

    /// 分辨率要是正数、竖握书写（短边在前），逻辑宽度与挖孔宽度也不能为负。
    pub fn is_valid(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.width <= self.height
            && self.points.unwrap_or(1) > 0
            && self.notch.unwrap_or(0) >= 0
    }

    /// 自定义分辨率：横向填写的自动转成竖向；越界返回 None。
    pub fn custom(a: Option<i32>, b: Option<i32>) -> Option<Self> {
        let (a, b) = (a?, b?);
        if !(1..=20000).contains(&a) || !(1..=20000).contains(&b) {
            return None;
        }
        Some(Self::new(a.min(b), a.max(b), None))
    }

    /// 转成档位计算用的规格。
    pub fn to_fit_spec(&self) -> PhoneSpec {
        PhoneSpec::new(self.width as f64, self.height as f64, self.points.map(|p| p as f64))
            .with_notch(self.notch.map(|n| n as f64))
    }

    /// 该规格按竖屏手机算出来的逻辑尺寸（基准宽度）。
    pub fn base_size(&self) -> Size {
        let fit = ScreenFit::from_phone(&self.to_fit_spec());
        fit.size(fit.base_width())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhoneModel {
    pub brand: String,
    pub name: String,
    /// 机型更名后仍用于解析已保存配置的旧名称。
    #[serde(default, rename = "legacyNames", skip_serializing_if = "Vec::is_empty")]
    pub legacy_names: Vec<String>,
    pub screen: PhoneScreenSpec,
    /// 来自机型库的哪一段（`phone` / `tablet` / `computer`）。
    #[serde(default)]
    pub kind: DeviceKind,
}

impl PhoneModel {
    pub fn id(&self) -> String {
        format!("{} {}", self.brand, self.name)
    }
}

/// 用户选中的机型：机型名 + 当时的分辨率（机型库改名或删除后仍能沿用数值）+ 设备种类。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhoneChoice {
    #[serde(default)]
    pub model: Option<String>,
    pub screen: PhoneScreenSpec,
    #[serde(default)]
    pub kind: DeviceKind,
}

impl Default for PhoneChoice {
    fn default() -> Self {
        Self::standard()
    }
}

impl PhoneChoice {
    pub fn standard() -> Self {
        Self {
            model: Some("Apple iPhone 17 Pro Max".to_string()),
            screen: PhoneScreenSpec::new(1320, 2868, Some(440)),
            kind: DeviceKind::Phone,
        }
    }

    /// C# `PhoneChoice.StandardTablet`。
    pub fn standard_tablet() -> Self {
        Self {
            model: Some("Apple iPad Pro 11 (M4)".to_string()),
            screen: PhoneScreenSpec::new(1668, 2420, Some(834)),
            kind: DeviceKind::Tablet,
        }
    }

    /// C# `PhoneChoice.StandardComputer`。
    pub fn standard_computer() -> Self {
        Self {
            model: Some("通用 1080p".to_string()),
            screen: PhoneScreenSpec::new(1080, 1920, None),
            kind: DeviceKind::Computer,
        }
    }

    pub fn from_model(model: &PhoneModel) -> Self {
        Self { model: Some(model.id()), screen: model.screen, kind: model.kind }
    }

    /// 换设备种类但保留机型名与分辨率（C# 的 `phone with { Kind = … }`）。
    pub fn with_kind(mut self, kind: DeviceKind) -> Self {
        self.kind = kind;
        self
    }
}

#[derive(Debug, Error)]
#[error("机型库：{0}")]
pub struct PhoneCatalogError(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Brand {
    pub name: String,
    pub aliases: Vec<String>,
    pub models: Vec<PhoneModel>,
    pub kind: DeviceKind,
}

impl Brand {
    /// 机型库里这段品牌的唯一键（C# `SectionId`）。
    pub fn section_id(&self) -> String {
        format!("{}/{}", self.kind.label(), self.name)
    }
}

#[derive(Debug, Clone, Default)]
pub struct PhoneCatalog {
    pub brands: Vec<Brand>,
}

#[derive(Deserialize)]
struct RawFile {
    #[serde(default)]
    brands: Option<Vec<RawBrand>>,
    #[serde(default)]
    phone: Option<Vec<RawBrand>>,
    #[serde(default)]
    tablet: Option<Vec<RawBrand>>,
    #[serde(default)]
    computer: Option<Vec<RawBrand>>,
}

impl RawFile {
    /// C# `file.Phone is not null || file.Tablet is not null || file.Computer is not null`。
    fn has_sections(&self) -> bool {
        self.phone.is_some() || self.tablet.is_some() || self.computer.is_some()
    }
}

#[derive(Deserialize)]
struct RawBrand {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    aliases: Option<Vec<String>>,
    #[serde(default)]
    models: Option<Vec<RawModel>>,
}

#[derive(Deserialize)]
struct RawModel {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "legacyNames")]
    legacy_names: Vec<String>,
    #[serde(default)]
    screen: Option<Vec<i32>>,
    #[serde(default)]
    points: Option<i32>,
    #[serde(default)]
    notch: Option<i32>,
}

impl PhoneCatalog {
    /// 一台设备都没有的空库（C# `PhoneCatalog.Empty`）。
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn from_json(json: &str) -> Result<Self, PhoneCatalogError> {
        let file: RawFile = serde_json::from_str(json)
            .map_err(|e| PhoneCatalogError(format!("解不开：{e}")))?;

        let mut seen_models = std::collections::HashSet::new();
        let mut current_model_ids = std::collections::HashSet::new();
        let mut seen_legacy_names = std::collections::HashSet::new();
        let mut brands = Vec::new();

        // 上游 v2.6 的 device-models.json 是分段结构，优先按段解析。
        if file.has_sections() {
            parse_section(
                file.phone,
                DeviceKind::Phone,
                &mut seen_models,
                &mut current_model_ids,
                &mut seen_legacy_names,
                &mut brands,
            )?;
            parse_section(
                file.tablet,
                DeviceKind::Tablet,
                &mut seen_models,
                &mut current_model_ids,
                &mut seen_legacy_names,
                &mut brands,
            )?;
            parse_section(
                file.computer,
                DeviceKind::Computer,
                &mut seen_models,
                &mut current_model_ids,
                &mut seen_legacy_names,
                &mut brands,
            )?;
            return Ok(Self { brands });
        }

        // 老的 phone-models.json 只有 brands，全部按手机处理。
        let raw_brands =
            file.brands.ok_or_else(|| PhoneCatalogError("缺少 phone / brands 字段".to_string()))?;
        parse_section(
            Some(raw_brands),
            DeviceKind::Phone,
            &mut seen_models,
            &mut current_model_ids,
            &mut seen_legacy_names,
            &mut brands,
        )?;
        Ok(Self { brands })
    }

    pub fn load_from_file(path: impl AsRef<std::path::Path>) -> Result<Self, PhoneCatalogError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| PhoneCatalogError(format!("读不到文件：{e}")))?;
        Self::from_json(&text)
    }

    pub fn models(&self) -> impl Iterator<Item = &PhoneModel> {
        self.brands.iter().flat_map(|b| b.models.iter())
    }

    /// C# `BrandsIn(kinds)`：空参数表示全部。
    pub fn brands_in(&self, kinds: &[DeviceKind]) -> Vec<&Brand> {
        if kinds.is_empty() {
            return self.brands.iter().collect();
        }
        self.brands.iter().filter(|b| kinds.contains(&b.kind)).collect()
    }

    /// 按机型名找（C# `FindModel(id)`，不限种类）。
    pub fn find_model(&self, id: &str) -> Option<&PhoneModel> {
        self.models().find(|m| m.id() == id)
    }

    /// 按机型名 + 种类找（C# `FindModel(id, kind)`）。
    pub fn find_model_of_kind(&self, id: &str, kind: DeviceKind) -> Option<&PhoneModel> {
        self.models().find(|m| m.id() == id && m.kind == kind)
    }

    /// 启动时按机型名核对机型库，命中就用库里的数值（机型库改名/删除时沿用存下的）。
    pub fn resolve(&self, choice: PhoneChoice) -> PhoneChoice {
        if let Some(model_id) = choice.model.as_deref()
            && let Some(model) = self.find_model(model_id).or_else(|| {
                let legacy_id = normalize(model_id);
                self.models().find(|model| {
                    model.legacy_names.iter().any(|name| {
                        normalize(&format!("{} {name}", model.brand)) == legacy_id
                    })
                })
            })
        {
            return PhoneChoice::from_model(model);
        }
        choice
    }

    /// 按品牌名/别名 + 机型名做包含匹配（忽略大小写与空白），不限种类。
    pub fn search(&self, query: &str) -> Vec<Brand> {
        self.search_in(query, None)
    }

    /// C# `Search(query, kinds)`：`kinds` 为 None 时搜全部。
    pub fn search_in(&self, query: &str, kinds: Option<&[DeviceKind]>) -> Vec<Brand> {
        let source: Vec<&Brand> = match kinds {
            Some(kinds) => self.brands.iter().filter(|b| kinds.contains(&b.kind)).collect(),
            None => self.brands.iter().collect(),
        };
        let q = normalize(query);
        if q.is_empty() {
            return source.into_iter().cloned().collect();
        }
        let mut matched = Vec::new();
        for brand in source {
            let mut brand_names: Vec<String> = vec![normalize(&brand.name)];
            brand_names.extend(brand.aliases.iter().map(|a| normalize(a)));
            let models: Vec<PhoneModel> = brand
                .models
                .iter()
                .filter(|model| {
                    let model_norm = normalize(&model.name);
                    brand_names.iter().any(|bn| format!("{bn}{model_norm}").contains(&q))
                })
                .cloned()
                .collect();
            if !models.is_empty() {
                matched.push(Brand {
                    name: brand.name.clone(),
                    aliases: brand.aliases.clone(),
                    models,
                    kind: brand.kind,
                });
            }
        }
        matched
    }
}

/// 解析一段品牌列表（C# `ParseSection`）。
fn parse_section(
    section: Option<Vec<RawBrand>>,
    kind: DeviceKind,
    seen_models: &mut std::collections::HashSet<String>,
    current_model_ids: &mut std::collections::HashSet<String>,
    seen_legacy_names: &mut std::collections::HashSet<String>,
    output: &mut Vec<Brand>,
) -> Result<(), PhoneCatalogError> {
    let Some(raw_brands) = section else { return Ok(()) };
    let mut seen_brands = std::collections::HashSet::new();

    for raw in raw_brands {
        let brand_name = raw.name.unwrap_or_default();
        let raw_models = raw.models.unwrap_or_default();
        if brand_name.is_empty() || raw_models.is_empty() {
            return Err(PhoneCatalogError(format!("品牌「{brand_name}」没有名字或没有机型")));
        }
        if !seen_brands.insert(normalize(&brand_name)) {
            return Err(PhoneCatalogError(format!(
                "{} 里品牌「{brand_name}」重复了",
                kind.label()
            )));
        }

        let mut models = Vec::with_capacity(raw_models.len());
        for entry in raw_models {
            let screen_pair = entry
                .screen
                .ok_or_else(|| PhoneCatalogError(format!("{brand_name}：缺少 screen 字段")))?;
            if screen_pair.len() != 2 {
                let name = entry.name.clone().unwrap_or_default();
                return Err(PhoneCatalogError(format!(
                    "{brand_name} {name}：screen 要写成 [短边, 长边]"
                )));
            }
            let screen = PhoneScreenSpec::new(screen_pair[0], screen_pair[1], entry.points)
                .with_notch(entry.notch);
            let model_name = entry.name.unwrap_or_default();
            let mut legacy_names = Vec::with_capacity(entry.legacy_names.len());
            for raw_name in entry.legacy_names {
                let name = raw_name.trim().to_string();
                if name.is_empty() {
                    return Err(PhoneCatalogError(format!(
                        "{brand_name} {model_name}：旧机型名不能为空"
                    )));
                }
                let legacy_id = normalize(&format!("{brand_name} {name}"));
                if normalize(&format!("{brand_name} {model_name}")) == legacy_id
                    || current_model_ids.contains(&legacy_id)
                    || !seen_legacy_names.insert(legacy_id)
                {
                    return Err(PhoneCatalogError(format!(
                        "{brand_name} {name}：旧机型名重复或与现有机型冲突"
                    )));
                }
                legacy_names.push(name);
            }
            let model = PhoneModel {
                brand: brand_name.clone(),
                name: model_name,
                legacy_names,
                screen,
                kind,
            };
            if model.name.is_empty() || !screen.is_valid() {
                return Err(PhoneCatalogError(format!(
                    "{}：分辨率要是正数、短边在前",
                    model.id()
                )));
            }
            if !seen_models.insert(normalize(&format!("{}/{}", kind.label(), model.id()))) {
                return Err(PhoneCatalogError(format!("{}：重复了", model.id())));
            }
            let model_id = normalize(&model.id());
            if seen_legacy_names.contains(&model_id) {
                return Err(PhoneCatalogError(format!(
                    "{}：机型名与已登记的旧机型名冲突",
                    model.id()
                )));
            }
            current_model_ids.insert(model_id);
            models.push(model);
        }

        output.push(Brand {
            name: brand_name,
            aliases: raw.aliases.unwrap_or_default(),
            models,
            kind,
        });
    }
    Ok(())
}

/// 小写 + 去掉所有空白（对应 C# `PhoneCatalog.Normalize`）。
pub fn normalize(text: &str) -> String {
    text.chars()
        .flat_map(|c| c.to_lowercase())
        .filter(|c| !c.is_whitespace())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATALOG: &str = r#"{
      "brands": [
        {
          "name": "Apple",
          "aliases": ["苹果"],
          "models": [
            { "name": "iPhone 17 Pro Max", "screen": [1320, 2868], "points": 440 },
            { "name": "iPhone SE", "screen": [750, 1334], "points": 375 }
          ]
        },
        {
          "name": "Xiaomi",
          "models": [
            { "name": "Xiaomi 15", "screen": [1200, 2670] }
          ]
        }
      ]
    }"#;

    /// v1.3 的三段式结构：每种设备一段，机型自动带 kind。
    const SECTIONS: &str = r#"{
      "phone": [
        {"name": "Apple", "aliases": ["苹果"], "models": [
          { "name": "iPhone 17 Pro Max", "screen": [1320, 2868], "points": 440 }
        ]}
      ],
      "tablet": [
        {"name": "Apple", "models": [
          { "name": "iPad Pro 11 (M4)", "screen": [1668, 2420], "points": 834 }
        ]}
      ],
      "computer": [
        {"name": "通用", "aliases": ["Windows"], "models": [
          { "name": "1920 × 1080", "screen": [1080, 1920] },
          { "name": "带挖孔 1920 × 1080", "screen": [1080, 1920], "notch": 60 }
        ]}
      ]
    }"#;

    #[test]
    fn catalog_parses_and_indexes_models() {
        let cat = PhoneCatalog::from_json(CATALOG).expect("catalog");
        assert_eq!(cat.brands.len(), 2);
        assert_eq!(cat.models().count(), 3);
        let model = cat.find_model("Apple iPhone 17 Pro Max").expect("model");
        assert_eq!(model.screen.width, 1320);
        assert_eq!(model.screen.points, Some(440));
        assert_eq!(model.kind, DeviceKind::Phone, "旧 brands 结构默认按手机处理");
    }

    #[test]
    fn sectioned_catalog_tags_each_brand_with_its_device_kind() {
        let cat = PhoneCatalog::from_json(SECTIONS).expect("sections");
        assert_eq!(cat.models().count(), 4);
        assert_eq!(cat.brands_in(&[DeviceKind::Phone]).len(), 1);
        assert_eq!(cat.brands_in(&[DeviceKind::Tablet]).len(), 1);
        assert_eq!(cat.brands_in(&[DeviceKind::Computer]).len(), 1);
        assert_eq!(cat.brands_in(&[]).len(), 3, "空参数表示全部");

        let tablet = cat.find_model("Apple iPad Pro 11 (M4)").expect("tablet");
        assert_eq!(tablet.kind, DeviceKind::Tablet);
        assert!(cat.find_model_of_kind("Apple iPad Pro 11 (M4)", DeviceKind::Tablet).is_some());
        assert!(cat.find_model_of_kind("Apple iPad Pro 11 (M4)", DeviceKind::Phone).is_none());

        let notched = cat.find_model("通用 带挖孔 1920 × 1080").expect("notched");
        assert_eq!(notched.kind, DeviceKind::Computer);
        assert_eq!(notched.screen.notch, Some(60));
    }

    #[test]
    fn search_can_be_limited_to_one_device_kind() {
        let cat = PhoneCatalog::from_json(SECTIONS).expect("sections");
        let tablets = cat.search_in("iPad", Some(&[DeviceKind::Tablet]));
        assert_eq!(tablets.len(), 1);
        assert!(tablets.iter().all(|b| b.kind == DeviceKind::Tablet));
        assert!(cat.search_in("iPad", Some(&[DeviceKind::Phone])).is_empty());
        assert_eq!(
            cat.search_in("1080", Some(&[DeviceKind::Computer]))[0].models.len(),
            2
        );
    }

    #[test]
    fn standard_choice_matches_catalog_entry() {
        let cat = PhoneCatalog::from_json(CATALOG).expect("catalog");
        let resolved = cat.resolve(PhoneChoice::standard());
        assert_eq!(resolved.screen, PhoneScreenSpec::new(1320, 2868, Some(440)));
        assert_eq!(resolved.model.as_deref(), Some("Apple iPhone 17 Pro Max"));
        assert_eq!(resolved.kind, DeviceKind::Phone);
    }

    #[test]
    fn legacy_model_name_resolves_to_the_current_catalog_entry() {
        let catalog = PhoneCatalog::from_json(
            r#"{"tablet":[{"name":"Apple","models":[{"name":"iPad mini（第 6 代）","legacyNames":["iPad mini (6th generation)"],"screen":[1488,2266],"points":744}]}]}"#,
        )
        .expect("catalog with a rename mapping");
        let choice = PhoneChoice {
            model: Some(" Apple   iPad mini (6th generation) ".to_string()),
            screen: PhoneScreenSpec::new(1200, 2000, None),
            kind: DeviceKind::Phone,
        };

        let resolved = catalog.resolve(choice);
        assert_eq!(resolved.model.as_deref(), Some("Apple iPad mini（第 6 代）"));
        assert_eq!(resolved.screen, PhoneScreenSpec::new(1488, 2266, Some(744)));
        assert_eq!(resolved.kind, DeviceKind::Tablet);
    }

    #[test]
    fn bundled_catalog_migrates_upstream_ipad_model_names() {
        let catalog = PhoneCatalog::from_json(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/phone-models.json"
        )))
        .expect("bundled catalog");
        let choice = PhoneChoice {
            model: Some("Apple iPad mini (6th generation)".to_string()),
            screen: PhoneScreenSpec::new(1488, 2266, Some(744)),
            kind: DeviceKind::Phone,
        };

        let resolved = catalog.resolve(choice);
        assert_eq!(resolved.model.as_deref(), Some("Apple iPad mini（第 6 代）"));
        assert_eq!(resolved.kind, DeviceKind::Tablet);
        assert_eq!(resolved.screen, PhoneScreenSpec::new(1488, 2266, Some(744)));
    }

    #[test]
    fn legacy_model_names_must_be_non_empty_and_unique() {
        let empty = r#"{"phone":[{"name":"Apple","models":[{"name":"iPhone","legacyNames":["   "],"screen":[10,20]}]}]}"#;
        assert!(PhoneCatalog::from_json(empty).unwrap_err().0.contains("旧机型名不能为空"));

        let duplicate = r#"{"phone":[{"name":"Apple","models":[{"name":"New A","legacyNames":["Old Name"],"screen":[10,20]},{"name":"New B","legacyNames":[" old   name "],"screen":[10,20]}]}]}"#;
        assert!(PhoneCatalog::from_json(duplicate).unwrap_err().0.contains("旧机型名重复"));

        let collides_with_current = r#"{"phone":[{"name":"Apple","models":[{"name":"New Name","legacyNames":["Current"],"screen":[10,20]},{"name":"Current","screen":[10,20]}]}]}"#;
        assert!(PhoneCatalog::from_json(collides_with_current).unwrap_err().0.contains("机型名与已登记的旧机型名冲突"));

        let collides_with_previous_current = r#"{"phone":[{"name":"Apple","models":[{"name":"Current","screen":[10,20]},{"name":"New Name","legacyNames":[" current "],"screen":[10,20]}]}]}"#;
        assert!(PhoneCatalog::from_json(collides_with_previous_current).unwrap_err().0.contains("旧机型名重复"));
    }

    #[test]
    fn unknown_model_keeps_stored_screen() {
        let cat = PhoneCatalog::from_json(CATALOG).expect("catalog");
        let choice = PhoneChoice {
            model: Some("Apple iPhone 99".to_string()),
            screen: PhoneScreenSpec::new(1080, 2400, None),
            kind: DeviceKind::Phone,
        };
        assert_eq!(cat.resolve(choice.clone()), choice);
    }

    #[test]
    fn search_matches_brand_alias_and_model() {
        let cat = PhoneCatalog::from_json(CATALOG).expect("catalog");
        assert_eq!(cat.search("iphone").len(), 1);
        assert_eq!(cat.search("苹果").len(), 1);
        assert_eq!(cat.search("xiaomi15").len(), 1);
        assert_eq!(cat.search("  iPhone   SE ")[0].models.len(), 1);
        assert_eq!(cat.search("iphonese")[0].models[0].id(), "Apple iPhone SE");
        assert!(cat.search("nokia").is_empty());
        assert_eq!(cat.search("").len(), 2);
    }

    #[test]
    fn duplicate_and_invalid_entries_are_rejected() {
        let dup = r#"{"brands":[{"name":"A","models":[{"name":"M","screen":[10,20]}]},{"name":"a","models":[{"name":"N","screen":[10,20]}]}]}"#;
        assert!(PhoneCatalog::from_json(dup).is_err());

        let landscape = r#"{"brands":[{"name":"A","models":[{"name":"M","screen":[20,10]}]}]}"#;
        assert!(PhoneCatalog::from_json(landscape).is_err());

        let empty_models = r#"{"brands":[{"name":"A","models":[]}]}"#;
        assert!(PhoneCatalog::from_json(empty_models).is_err());

        // 同一机型不能同时出现在两段里（C# 用 "{Kind}/{Id}" 做去重键）。
        let across_sections = r#"{"phone":[{"name":"A","models":[{"name":"M","screen":[10,20]}]}],"tablet":[{"name":"A","models":[{"name":"M","screen":[10,20]}]}]}"#;
        assert!(PhoneCatalog::from_json(across_sections).is_ok(), "不同种类同名不算重复");

        let duplicated_in_section = r#"{"tablet":[{"name":"A","models":[{"name":"M","screen":[10,20]},{"name":"m","screen":[10,20]}]}]}"#;
        assert!(PhoneCatalog::from_json(duplicated_in_section).is_err());
    }

    #[test]
    fn custom_resolution_normalises_orientation_and_bounds() {
        assert_eq!(
            PhoneScreenSpec::custom(Some(2400), Some(1080)),
            Some(PhoneScreenSpec::new(1080, 2400, None))
        );
        assert_eq!(PhoneScreenSpec::custom(Some(0), Some(1080)), None);
        assert_eq!(PhoneScreenSpec::custom(Some(30000), Some(1080)), None);
        assert_eq!(PhoneScreenSpec::custom(None, Some(1080)), None);
    }

    #[test]
    fn negative_notch_makes_the_spec_invalid() {
        assert!(!PhoneScreenSpec::new(1080, 1920, None).with_notch(Some(-1)).is_valid());
        assert!(PhoneScreenSpec::new(1080, 1920, None).with_notch(Some(0)).is_valid());
    }

    #[test]
    fn device_kind_helpers_match_csharp_tables() {
        assert!(DeviceKind::Phone.is_touch());
        assert!(DeviceKind::Tablet.is_touch());
        assert!(!DeviceKind::Computer.is_touch());

        assert_eq!(DeviceKind::Phone.usual_orientation(), ScreenOrientation::Portrait);
        assert_eq!(DeviceKind::Tablet.usual_orientation(), ScreenOrientation::Landscape);
        assert_eq!(DeviceKind::Computer.usual_orientation(), ScreenOrientation::Landscape);

        assert_eq!(DeviceKind::Computer.allowed_orientations(), &[ScreenOrientation::Landscape]);
        assert_eq!(
            DeviceKind::Tablet.allowed_orientations(),
            &[ScreenOrientation::Portrait, ScreenOrientation::Landscape]
        );
        assert_eq!(DeviceKind::Phone.swappable_kinds(), &[DeviceKind::Phone, DeviceKind::Tablet]);
        assert_eq!(DeviceKind::Computer.swappable_kinds(), &[DeviceKind::Computer]);

        assert_eq!(DeviceKind::Phone.display_name(), "手机");
        assert_eq!(DeviceKind::Tablet.display_name(), "平板");
        assert_eq!(DeviceKind::Computer.display_name(), "电脑");
        assert_eq!(ScreenOrientation::Portrait.display_name(), "竖屏");
        assert_eq!(ScreenOrientation::Landscape.display_name(), "横屏");
    }

    #[test]
    fn real_catalog_asset_loads() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/phone-models.json");
        let cat = PhoneCatalog::load_from_file(path).expect("real catalog");
        assert!(cat.brands.len() >= 10, "brands={}", cat.brands.len());
        assert!(cat.models().count() >= 800, "models={}", cat.models().count());
        assert!(cat.find_model("Apple iPhone 17 Pro Max").is_some());

        // v1.3 的机型库分段：手机 / 平板 / 电脑都要有，否则 C# 那边选不到平板/副屏。
        let phones = cat.brands_in(&[DeviceKind::Phone]);
        let tablets = cat.brands_in(&[DeviceKind::Tablet]);
        let computers = cat.brands_in(&[DeviceKind::Computer]);
        assert!(!phones.is_empty() && !tablets.is_empty() && !computers.is_empty());
        assert!(
            cat.models().filter(|m| m.kind == DeviceKind::Tablet).count() > 20,
            "平板机型太少"
        );
        assert!(
            cat.models().filter(|m| m.kind == DeviceKind::Computer).count() > 5,
            "电脑机型太少"
        );
    }
}
