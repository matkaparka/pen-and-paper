// SPDX-License-Identifier: GPL-3.0-or-later
//! 快捷键：出厂键位表、用户改过的键位、从按键查命令（规格 F-150、F-153）。
//!
//! 键位存在设置文件里，一行一个：`key.undo = Ctrl+Z`，等号后面空着表示这个功能不绑键。

use crate::settings::Settings;
use crate::{Cmd, FilterKind, Tool};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Key {
    /// 虚拟键码
    pub vk: u16,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

/// 能绑的键和它们的名字。数字键单按是调浓度，带修饰键时才能绑。
const NAMED: &[(u16, &str)] = &[
    (0x09, "Tab"),
    (0x21, "PageUp"),
    (0x22, "PageDown"),
    (0x23, "End"),
    (0x24, "Home"),
    (0x2D, "Insert"),
    (0x2E, "Delete"),
    (0xBA, ";"),
    (0xBB, "="),
    (0xBC, ","),
    (0xBD, "-"),
    (0xBE, "."),
    (0xBF, "/"),
    (0xC0, "`"),
    (0xDB, "["),
    (0xDC, "\\"),
    (0xDD, "]"),
    (0xDE, "'"),
];

impl Key {
    fn base_name(vk: u16) -> Option<String> {
        match vk {
            0x30..=0x39 | 0x41..=0x5A => Some((vk as u8 as char).to_string()),
            0x70..=0x7B => Some(format!("F{}", vk - 0x6F)),
            _ => NAMED.iter().find(|n| n.0 == vk).map(|n| n.1.to_string()),
        }
    }

    /// 这个键能不能绑功能。单按数字键是调浓度，留着不让绑。
    pub fn bindable(&self) -> bool {
        Self::base_name(self.vk).is_some() && !((0x30..=0x39).contains(&self.vk) && !self.ctrl && !self.alt)
    }

    pub fn name(&self) -> String {
        let mut s = String::new();
        for (on, text) in [(self.ctrl, "Ctrl+"), (self.shift, "Shift+"), (self.alt, "Alt+")] {
            if on {
                s.push_str(text);
            }
        }
        s + &Self::base_name(self.vk).unwrap_or_default()
    }

    pub fn parse(text: &str) -> Option<Key> {
        let mut key = Key { vk: 0, ctrl: false, shift: false, alt: false };
        let mut rest = text.trim();
        loop {
            let lower = rest.to_ascii_lowercase();
            let (flag, len) = if lower.starts_with("ctrl+") {
                (&mut key.ctrl, 5)
            } else if lower.starts_with("shift+") {
                (&mut key.shift, 6)
            } else if lower.starts_with("alt+") {
                (&mut key.alt, 4)
            } else {
                break;
            };
            *flag = true;
            rest = &rest[len..];
        }
        let upper = rest.to_ascii_uppercase();
        key.vk = match upper.as_bytes() {
            [c @ (b'0'..=b'9' | b'A'..=b'Z')] => *c as u16,
            [b'F', n @ ..] if !n.is_empty() => {
                let n: u16 = upper[1..].parse().ok().filter(|n| (1..=12).contains(n))?;
                0x6F + n
            }
            _ => NAMED.iter().find(|n| n.1.eq_ignore_ascii_case(rest))?.0,
        };
        key.bindable().then_some(key)
    }
}

/// 一个可以绑键的功能：设置文件里的名字、界面上的中英文名字、命令、出厂键位（空串是不绑）。
pub struct Action {
    pub id: &'static str,
    label_zh: &'static str,
    label_en: &'static str,
    pub cmd: Cmd,
    pub default: &'static str,
}

const fn action(id: &'static str, label_zh: &'static str, label_en: &'static str, cmd: Cmd, default: &'static str) -> Action {
    Action { id, label_zh, label_en, cmd, default }
}

impl Action {
    /// 当前界面语言下的名字。
    pub fn label(&self) -> &'static str {
        zhibi_core::lang::tr(self.label_zh, self.label_en)
    }
}

/// 出厂键位表。常用的工具和导航都是单键（规格 F-150）。
pub const ACTIONS: &[Action] = &[
    action("pencil", "笔", "Pen", Cmd::SelectTool(Tool::Pen), "B"),
    action("marker", "马克笔", "Marker", Cmd::SelectTool(Tool::Marker), "N"),
    action("airbrush", "喷枪", "Airbrush", Cmd::SelectTool(Tool::Airbrush), "A"),
    action("eraser", "橡皮", "Eraser", Cmd::SelectTool(Tool::Eraser), "E"),
    action("bucket", "油漆桶", "Bucket", Cmd::SelectTool(Tool::Bucket), "G"),
    action("select", "选框", "Rectangle Select", Cmd::SelectTool(Tool::Select), "M"),
    action("lasso", "套索", "Lasso", Cmd::SelectTool(Tool::Lasso), "L"),
    action("wand", "魔棒", "Magic Wand", Cmd::SelectTool(Tool::Wand), "W"),
    action("move", "移动", "Move", Cmd::SelectTool(Tool::Move), "V"),
    action("liquify", "液化", "Liquify", Cmd::SelectTool(Tool::Liquify), "R"),
    action("size_down", "笔刷小一档", "Smaller Brush", Cmd::SizeDown, "["),
    action("size_up", "笔刷大一档", "Bigger Brush", Cmd::SizeUp, "]"),
    action("stabilizer_down", "稳定器低一档", "Less Stabilizing", Cmd::StabilizerDown, "F2"),
    action("stabilizer_up", "稳定器高一档", "More Stabilizing", Cmd::StabilizerUp, "F3"),
    action("swap_colors", "交换前景色和背景色", "Swap Colors", Cmd::SwapColors, "X"),
    action("undo", "撤销", "Undo", Cmd::Undo, "Ctrl+Z"),
    action("redo", "重做", "Redo", Cmd::Redo, "Ctrl+Y"),
    action("redo_2", "重做（第二个键）", "Redo (second key)", Cmd::Redo, "Ctrl+Shift+Z"),
    action("clear", "清除", "Clear", Cmd::ClearLayer, "Delete"),
    action("fill", "填充前景色", "Fill with Foreground Color", Cmd::FillLayer, ""),
    action("transform", "自由变换", "Free Transform", Cmd::Transform, "Ctrl+T"),
    action("hue_sat", "色相/饱和度", "Hue/Saturation", Cmd::Filter(FilterKind::HueSat), "Ctrl+U"),
    action("bright_contrast", "亮度/对比度", "Brightness/Contrast", Cmd::Filter(FilterKind::BrightContrast), ""),
    action("blur", "模糊", "Blur", Cmd::Filter(FilterKind::Blur), ""),
    action("select_all", "全选", "Select All", Cmd::SelectAll, "Ctrl+A"),
    action("deselect", "取消选择", "Deselect", Cmd::Deselect, "Ctrl+D"),
    action("invert_selection", "反选", "Invert Selection", Cmd::InvertSelection, "Ctrl+Shift+I"),
    action("grow", "选区扩大 1 像素", "Grow Selection by 1 px", Cmd::Grow, ""),
    action("shrink", "选区收缩 1 像素", "Shrink Selection by 1 px", Cmd::Shrink, ""),
    action("outline", "显示或隐藏选区边线", "Show or Hide Selection Outline", Cmd::ToggleOutline, "Ctrl+H"),
    action("zoom_in", "放大", "Zoom In", Cmd::ZoomIn, "PageUp"),
    action("zoom_out", "缩小", "Zoom Out", Cmd::ZoomOut, "PageDown"),
    action("zoom_100", "实际大小", "Actual Size", Cmd::Zoom100, "Ctrl+Alt+0"),
    action("fit", "适合窗口", "Fit to Window", Cmd::Fit, "Ctrl+0"),
    action("default_view", "回到全图", "Reset View", Cmd::DefaultView, "Home"),
    action("angle_zero", "角度归零", "Reset Rotation", Cmd::AngleZero, "Shift+Home"),
    action("view_ccw", "视图左转", "Rotate View Left", Cmd::RotateCcw, ""),
    action("view_cw", "视图右转", "Rotate View Right", Cmd::RotateCw, ""),
    action("mirror", "镜像查看", "Mirror View", Cmd::Flip, "H"),
    action("panels", "隐藏或显示面板", "Show or Hide Panels", Cmd::TogglePanels, "Tab"),
    action("layer_new", "新建图层", "New Layer", Cmd::LayerNew, "Ctrl+Shift+N"),
    action("layer_dup", "复制图层", "Duplicate Layer", Cmd::LayerDup, "Ctrl+J"),
    action("folder_new", "新建组", "New Group", Cmd::FolderNew, ""),
    action("group", "放进新组", "Put into New Group", Cmd::Group, "Ctrl+G"),
    action("mask_add", "添加蒙版", "Add Mask", Cmd::MaskAdd, ""),
    action("mask_delete", "删除蒙版", "Delete Mask", Cmd::MaskDelete, ""),
    action("mask_toggle", "停用或启用蒙版", "Disable or Enable Mask", Cmd::MaskToggle, ""),
    action("mask_apply", "应用蒙版", "Apply Mask", Cmd::MaskApply, ""),
    action("mask_edit", "在蒙版和像素之间切换", "Edit Mask or Pixels", Cmd::MaskEdit, ""),
    action("fill_color", "填充图层改用前景色", "Fill Layer: Use Foreground Color", Cmd::FillColor, ""),
    action("rasterize", "栅格化填充图层", "Rasterize Fill Layer", Cmd::Rasterize, ""),
    action("style_toggle", "打开或关掉图层样式", "Layer Style On or Off", Cmd::StyleToggle, ""),
    action("layer_del", "删除图层", "Delete Layer", Cmd::LayerDel, ""),
    action("layer_up", "图层上移", "Move Layer Up", Cmd::LayerUp, ""),
    action("layer_down", "图层下移", "Move Layer Down", Cmd::LayerDown, ""),
    action("merge_down", "向下合并", "Merge Down", Cmd::LayerMerge, "Ctrl+E"),
    action("merge_visible", "合并可见图层", "Merge Visible", Cmd::MergeVisible, ""),
    action("flatten", "拼合成一层", "Flatten", Cmd::Flatten, ""),
    action("white_to_alpha", "白底变透明", "White to Transparent", Cmd::WhiteToAlpha, ""),
    action("lock_alpha", "保护不透明度", "Protect Opacity", Cmd::ToggleLockAlpha, ""),
    action("lock", "锁定图层", "Lock Layer", Cmd::ToggleLocked, ""),
    action("clip", "剪贴到下面的图层", "Clip to Layer Below", Cmd::ToggleClip, ""),
    action("flip_h", "左右翻转", "Flip Horizontal", Cmd::FlipH, ""),
    action("flip_v", "上下翻转", "Flip Vertical", Cmd::FlipV, ""),
    action("turn_ccw", "左转 90°", "Rotate 90° Left", Cmd::TurnCcw, ""),
    action("turn_cw", "右转 90°", "Rotate 90° Right", Cmd::TurnCw, ""),
    action("new", "新建", "New", Cmd::New, "Ctrl+N"),
    action("open", "打开", "Open", Cmd::Open, "Ctrl+O"),
    action("save", "保存", "Save", Cmd::Save, "Ctrl+S"),
    action("save_as", "另存为", "Save As", Cmd::SaveAs, "Ctrl+Shift+S"),
    action("export", "导出", "Export", Cmd::Export, "Ctrl+Shift+E"),
];

/// 当前的键位：与 `ACTIONS` 一一对应。
#[derive(Clone, PartialEq)]
pub struct Keymap {
    keys: Vec<Option<Key>>,
}

impl Keymap {
    pub fn factory() -> Self {
        Self { keys: ACTIONS.iter().map(|a| Key::parse(a.default)).collect() }
    }

    pub fn load(settings: &Settings) -> Self {
        let mut map = Self::factory();
        for (slot, a) in map.keys.iter_mut().zip(ACTIONS) {
            if let Some(text) = settings.text(&format!("key.{}", a.id)) {
                *slot = Key::parse(text);
            }
        }
        // 设置文件可能被手改出重复的键：靠前的功能留着，后面重复的解除。
        for i in 0..map.keys.len() {
            if map.keys[i].is_some() && map.keys[..i].contains(&map.keys[i]) {
                map.keys[i] = None;
            }
        }
        map
    }

    /// 只把和出厂不一样的写进设置，文件里看得出哪些是自己改过的。
    pub fn save(&self, settings: &mut Settings) {
        for (key, a) in self.keys.iter().zip(ACTIONS) {
            if *key != Key::parse(a.default) {
                settings.set(&format!("key.{}", a.id), key.map(|k| k.name()).unwrap_or_default());
            }
        }
    }

    pub fn get(&self, index: usize) -> Option<Key> {
        self.keys.get(index).copied().flatten()
    }

    /// 给第 `index` 个功能绑键（None 是解除）。这个键原来绑着别的功能的话，那一个被解除，返回它的序号。
    pub fn set(&mut self, index: usize, key: Option<Key>) -> Option<usize> {
        let taken = key.and_then(|k| self.keys.iter().position(|slot| *slot == Some(k))).filter(|&i| i != index);
        if let Some(i) = taken {
            self.keys[i] = None;
        }
        self.keys[index] = key;
        taken
    }

    pub fn find(&self, key: Key) -> Option<Cmd> {
        self.keys.iter().position(|slot| *slot == Some(key)).map(|i| ACTIONS[i].cmd)
    }

    /// 某个命令现在绑的键的名字，给菜单显示用。
    pub fn hint(&self, cmd: Cmd) -> Option<String> {
        self.keys.iter().zip(ACTIONS).find(|(key, a)| a.cmd == cmd && key.is_some()).and_then(|(key, _)| key.map(|k| k.name()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for text in ["B", "Ctrl+Z", "Ctrl+Shift+S", "Ctrl+Alt+0", "Shift+Home", "PageUp", "F3", "F12", "[", "]", "Tab", "Delete"] {
            let key = Key::parse(text).unwrap_or_else(|| panic!("{text}"));
            assert_eq!(key.name(), text);
        }
        assert_eq!(Key::parse("ctrl+shift+s"), Key::parse("Ctrl+Shift+S"));
        // 单按数字键留给浓度；不认识的名字、空串都不是键。
        assert_eq!(Key::parse("5"), None);
        assert!(Key::parse("Ctrl+5").is_some());
        for bad in ["", "Ctrl+", "F13", "F0", "Enter", "Esc", "Space", "ab"] {
            assert_eq!(Key::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn factory_table_is_consistent() {
        let map = Keymap::factory();
        for (i, a) in ACTIONS.iter().enumerate() {
            assert_eq!(a.default.is_empty(), map.get(i).is_none(), "{} 的出厂键位写错了", a.id);
            assert!(!ACTIONS[..i].iter().any(|b| b.id == a.id), "{} 重名", a.id);
            if let Some(key) = map.get(i) {
                assert!(!(0..i).any(|j| map.get(j) == Some(key)), "{} 的键和别的功能重了", a.id);
            }
        }
        // 常用的工具和导航都能单键完成（规格 F-150）。
        let single = (0..ACTIONS.len()).filter_map(|i| map.get(i)).filter(|k| !k.ctrl && !k.alt && !k.shift).count();
        assert!(single >= 18, "{single}");
    }

    #[test]
    fn rebinding_takes_the_key_away_from_its_old_owner() {
        let mut map = Keymap::factory();
        let index = |id: &str| ACTIONS.iter().position(|a| a.id == id).unwrap();
        let e = Key::parse("E").unwrap();
        assert_eq!(map.find(e), Some(Cmd::SelectTool(Tool::Eraser)));
        assert_eq!(map.set(index("bucket"), Some(e)), Some(index("eraser")));
        assert_eq!(map.find(e), Some(Cmd::SelectTool(Tool::Bucket)));
        assert_eq!(map.get(index("eraser")), None);
        assert_eq!(map.find(Key::parse("G").unwrap()), None);
        // 再绑同一个键不算抢。
        assert_eq!(map.set(index("bucket"), Some(e)), None);
        assert_eq!(map.set(index("bucket"), None), None);
        assert_eq!(map.find(e), None);
    }

    #[test]
    fn only_changes_are_written_and_read_back() {
        let mut map = Keymap::factory();
        let index = |id: &str| ACTIONS.iter().position(|a| a.id == id).unwrap();
        map.set(index("layer_del"), Key::parse("Ctrl+Delete"));
        map.set(index("mirror"), None);
        let mut s = Settings::default();
        map.save(&mut s);
        assert_eq!(s.text("key.layer_del"), Some("Ctrl+Delete"));
        assert_eq!(s.text("key.mirror"), Some(""));
        assert_eq!(s.text("key.undo"), None);
        assert!(Keymap::load(&s) == map);
        assert_eq!(map.hint(Cmd::LayerDel).as_deref(), Some("Ctrl+Delete"));
        assert_eq!(map.hint(Cmd::Flip), None);
        // 两个键的功能，菜单上显示第一个。
        assert_eq!(map.hint(Cmd::Redo).as_deref(), Some("Ctrl+Y"));
    }
}
