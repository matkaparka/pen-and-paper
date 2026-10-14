// SPDX-License-Identifier: GPL-3.0-or-later
//! 设置文件：一个纯文本文件，每行 `键 = 值`。删掉它，下次启动就回到出厂状态（规格 F-163）。

use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Default)]
pub struct Settings {
    values: BTreeMap<String, String>,
}

/// `%APPDATA%\Zhibi\settings.txt`
pub fn path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("Zhibi").join("settings.txt"))
}

impl Settings {
    pub fn load() -> Self {
        let mut s = Self::default();
        let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else { return s };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                s.values.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        s
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut text = String::from("# 纸笔（Pen & Paper）的设置。删掉这个文件即可恢复出厂状态。\n# Pen & Paper settings. Delete this file to restore the defaults.\n");
        for (k, v) in &self.values {
            text.push_str(&format!("{k} = {v}\n"));
        }
        let _ = std::fs::write(p, text);
    }

    pub fn f32(&self, key: &str, default: f32) -> f32 {
        self.values.get(key).and_then(|v| v.parse().ok()).filter(|v: &f32| v.is_finite()).unwrap_or(default)
    }

    pub fn usize(&self, key: &str, default: usize) -> usize {
        self.values.get(key).and_then(|v| v.parse().ok()).unwrap_or(default)
    }

    pub fn text(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(|v| v.as_str())
    }

    pub fn set(&mut self, key: &str, value: impl ToString) {
        self.values.insert(key.to_string(), value.to_string());
    }
}
