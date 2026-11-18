//! 机型库，移植自 C# 版 `SnugWin.Core.PhoneScreen.PhoneModels`。
//!
//! 数据源就是 `assets/phone-models.json`（17 个品牌、约 800 款机型），
//! 竖握书写：短边在前。校验规则与 C# 版一致，出错时给出中文原因。

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::geometry::Size;
use crate::screen_fit::{PhoneSpec, ScreenFit};

/// 机型条目里的屏幕规格。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhoneScreenSpec {
    pub width: i32,
    pub height: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub points: Option<i32>,
}

impl PhoneScreenSpec {
    pub const fn new(width: i32, height: i32, points: Option<i32>) -> Self {
        Self { width, height, points }
    }

    /// 分辨率要是正数、竖握书写（短边在前），有逻辑宽度时也要为正。
    pub fn is_valid(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.width <= self.height
            && self.points.unwrap_or(1) > 0
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
    }

    /// 该规格对应的逻辑尺寸（基准宽度）。
    pub fn base_size(&self) -> Size {
        ScreenFit::from_phone(&self.to_fit_spec()).size(
            ScreenFit::from_phone(&self.to_fit_spec()).base_width(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhoneModel {
    pub brand: String,
    pub name: String,
    pub screen: PhoneScreenSpec,
}

impl PhoneModel {
    pub fn id(&self) -> String {
        format!("{} {}", self.brand, self.name)
    }
}

/// 用户选中的机型：机型名 + 当时的分辨率（机型库改名或删除后仍能沿用数值）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhoneChoice {
    #[serde(default)]
    pub model: Option<String>,
    pub screen: PhoneScreenSpec,
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
        }
    }

    pub fn from_model(model: &PhoneModel) -> Self {
        Self { model: Some(model.id()), screen: model.screen }
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
}

#[derive(Debug, Clone, Default)]
pub struct PhoneCatalog {
    pub brands: Vec<Brand>,
}

#[derive(Deserialize)]
struct RawFile {
    #[serde(default)]
    brands: Option<Vec<RawBrand>>,
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
    #[serde(default)]
    screen: Option<Vec<i32>>,
    #[serde(default)]
    points: Option<i32>,
}

impl PhoneCatalog {
    pub fn from_json(json: &str) -> Result<Self, PhoneCatalogError> {
        let file: RawFile = serde_json::from_str(json)
            .map_err(|e| PhoneCatalogError(format!("解不开：{e}")))?;
        let raw_brands = file
            .brands
            .ok_or_else(|| PhoneCatalogError("缺少 brands 字段".to_string()))?;

        let mut seen_models = std::collections::HashSet::new();
        let mut seen_brands = std::collections::HashSet::new();
        let mut brands = Vec::with_capacity(raw_brands.len());

        for raw in raw_brands {
            let brand_name = raw.name.unwrap_or_default();
            let raw_models = raw.models.unwrap_or_default();
            if brand_name.is_empty() || raw_models.is_empty() {
                return Err(PhoneCatalogError(format!(
                    "品牌「{brand_name}」没有名字或没有机型"
                )));
            }
            if !seen_brands.insert(normalize(&brand_name)) {
                return Err(PhoneCatalogError(format!("品牌「{brand_name}」重复了")));
            }

            let mut models = Vec::with_capacity(raw_models.len());
            for entry in raw_models {
                let screen_pair = entry
                    .screen
                    .ok_or_else(|| PhoneCatalogError(format!("{brand_name}：缺少 screen 字段")))?;
                if screen_pair.len() != 2 {
                    let name = entry.name.clone().unwrap_or_default();
                    return Err(PhoneCatalogError(format!(
                        "{brand_name} {name}：screen 要写成 [宽, 高]"
                    )));
                }
                let screen = PhoneScreenSpec::new(screen_pair[0], screen_pair[1], entry.points);
                let model = PhoneModel {
                    brand: brand_name.clone(),
                    name: entry.name.unwrap_or_default(),
                    screen,
                };
                if model.name.is_empty() || !screen.is_valid() {
                    return Err(PhoneCatalogError(format!(
                        "{}：分辨率要是正数、按竖握写（短边在前）",
                        model.id()
                    )));
                }
                if !seen_models.insert(normalize(&model.id())) {
                    return Err(PhoneCatalogError(format!("{}：重复了", model.id())));
                }
                models.push(model);
            }

            brands.push(Brand {
                name: brand_name,
                aliases: raw.aliases.unwrap_or_default(),
                models,
            });
        }

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

    pub fn find_model(&self, id: &str) -> Option<&PhoneModel> {
        self.models().find(|m| m.id() == id)
    }

    /// 启动时按机型名核对机型库，命中就用库里的数值（机型库改名/删除时沿用存下的）。
    pub fn resolve(&self, choice: PhoneChoice) -> PhoneChoice {
        if let Some(model_id) = choice.model.as_deref()
            && let Some(model) = self.find_model(model_id)
        {
            return PhoneChoice::from_model(model);
        }
        choice
    }

    /// 按品牌名/别名 + 机型名做包含匹配（忽略大小写与空白）。
    pub fn search(&self, query: &str) -> Vec<Brand> {
        let q = normalize(query);
        if q.is_empty() {
            return self.brands.clone();
        }
        let mut matched = Vec::new();
        for brand in &self.brands {
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
                matched.push(Brand { name: brand.name.clone(), aliases: brand.aliases.clone(), models });
            }
        }
        matched
    }
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

    #[test]
    fn catalog_parses_and_indexes_models() {
        let cat = PhoneCatalog::from_json(CATALOG).expect("catalog");
        assert_eq!(cat.brands.len(), 2);
        assert_eq!(cat.models().count(), 3);
        let model = cat.find_model("Apple iPhone 17 Pro Max").expect("model");
        assert_eq!(model.screen.width, 1320);
        assert_eq!(model.screen.points, Some(440));
    }

    #[test]
    fn standard_choice_matches_catalog_entry() {
        let cat = PhoneCatalog::from_json(CATALOG).expect("catalog");
        let resolved = cat.resolve(PhoneChoice::standard());
        assert_eq!(resolved.screen, PhoneScreenSpec::new(1320, 2868, Some(440)));
        assert_eq!(resolved.model.as_deref(), Some("Apple iPhone 17 Pro Max"));
    }

    #[test]
    fn unknown_model_keeps_stored_screen() {
        let cat = PhoneCatalog::from_json(CATALOG).expect("catalog");
        let choice = PhoneChoice {
            model: Some("Apple iPhone 99".to_string()),
            screen: PhoneScreenSpec::new(1080, 2400, None),
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
    }

    #[test]
    fn custom_resolution_normalises_orientation_and_bounds() {
        assert_eq!(PhoneScreenSpec::custom(Some(2400), Some(1080)), Some(PhoneScreenSpec::new(1080, 2400, None)));
        assert_eq!(PhoneScreenSpec::custom(Some(0), Some(1080)), None);
        assert_eq!(PhoneScreenSpec::custom(Some(30000), Some(1080)), None);
        assert_eq!(PhoneScreenSpec::custom(None, Some(1080)), None);
    }

    #[test]
    fn real_catalog_asset_loads() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/phone-models.json");
        let cat = PhoneCatalog::load_from_file(path).expect("real catalog");
        assert!(cat.brands.len() >= 10, "brands={}", cat.brands.len());
        assert!(cat.models().count() >= 700, "models={}", cat.models().count());
        assert!(cat.find_model("Apple iPhone 17 Pro Max").is_some());
    }
}
