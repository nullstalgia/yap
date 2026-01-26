use crokey::KeyCombination;
use itertools::Itertools;
use ratatui::{
    prelude::*,
    widgets::{Block, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};
use ratatui_macros::{line, span};

#[cfg(feature = "macros")]
use crate::macros::MacroNameTag;
use crate::{
    app::App,
    keybinds::{Action, Keybinds},
};

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, strum::EnumIs)]
enum ActionOption {
    Recognized(Action),
    Unrecognized(String),
    // empty keybinds won't appear due to them not being retained during deserialization
    // Empty,
}

// impl ActionOption {
//     fn meow_ordering(&self, rhs: &Self) -> std::cmp::Ordering {
//         let inter_action_ordering = matches!(
//             (self, rhs),
//             (
//                 ActionOption::Recognized(Action::BuiltinAction(_)),
//                 ActionOption::Recognized(Action::BuiltinAction(_))
//             )
//         );

//         if inter_action_ordering {
//             let ActionOption::Recognized(Action::BuiltinAction(lhs_action)) = &self else {
//                 unreachable!()
//             };
//             let ActionOption::Recognized(Action::BuiltinAction(rhs_action)) = &rhs else {
//                 unreachable!()
//             };
//             let lhs: &str = rhs_action.as_ref();
//             let rhs: &str = lhs_action.as_ref();

//             lhs.cmp(rhs)
//         } else {
//             self.cmp(rhs)
//         }
//     }
// }

impl std::fmt::Display for ActionOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActionOption::Recognized(action) => match action {
                Action::BuiltinAction(action) => write!(f, "{action}"),

                #[cfg(feature = "macros")]
                Action::MacroInvocation(MacroNameTag {
                    name,
                    category: Some(category),
                }) => write!(f, "[Macro] {name} ({category})"),
                #[cfg(feature = "macros")]
                Action::MacroInvocation(MacroNameTag {
                    name,
                    category: None,
                }) => write!(f, "[Macro] {name}"),

                #[cfg(feature = "espflash")]
                Action::EspFlashProfile(profile) => write!(f, "[ESP] {profile}"),

                Action::Pause(duration) => write!(f, "Pause: {duration:?}"),
            },
            ActionOption::Unrecognized(unk) => write!(f, "?{unk}?"),
            // ActionOption::Empty => write!(f, "!Empty!"),
        }
    }
}

pub fn show_keybinds(
    keybinds: &Keybinds,
    scroll: &mut u16,
    frame: &mut Frame,
    area: Rect,
    app: &App,
) {
    let mut max_keycombo_length = 0;
    let mut max_line_length = 0;

    let (all_single_actions, all_chains): (Vec<_>, Vec<_>) = keybinds
        .keybindings
        .iter()
        // Sort all by the first action's actual string
        .sorted_by(|a, b| {
            let a_first = &a.1[0];
            let b_first = &b.1[0];
            Ord::cmp(b_first, a_first)
        })
        .map(|(kc, v)| {
            let kc_len = kc.to_string().len();
            max_keycombo_length = max_keycombo_length.max(kc_len);

            let actions: Vec<_> = v
                .iter()
                .map(|a| match app.get_action_from_string(a.as_str()) {
                    Some(action) => ActionOption::Recognized(action),
                    None => ActionOption::Unrecognized(a.to_owned()),
                })
                .collect();
            (kc, actions)
        })
        .partition(|(_, v)| v.len() == 1);

    let mut rows: Vec<Line<'static>> = Vec::new();

    // rows.push(Line::raw("").centered());

    let key_combo_style = |action_opt: &ActionOption| -> Style {
        match action_opt {
            ActionOption::Recognized(_) => Style::new(),
            ActionOption::Unrecognized(_) => Color::Yellow.into(),
        }
    };

    let action_style = |action_opt: &ActionOption| -> Style {
        match action_opt {
            ActionOption::Recognized(Action::Pause(_)) => Color::LightBlue.into(),
            #[cfg(feature = "macros")]
            ActionOption::Recognized(Action::MacroInvocation(_)) => Color::Green.into(),
            #[cfg(feature = "espflash")]
            ActionOption::Recognized(Action::EspFlashProfile(_)) => Color::Magenta.into(),
            ActionOption::Recognized(_) => Color::Cyan.into(),
            ActionOption::Unrecognized(_) => Color::Yellow.into(),
        }
    };

    #[allow(clippy::type_complexity)]
    // Eh, isn't *that* bad.
    let (single_action_binds, unknown_single_actions): (
        Vec<(&KeyCombination, ActionOption)>,
        Vec<(&KeyCombination, ActionOption)>,
    ) = all_single_actions
        .into_iter()
        .map(|(kc, mut v)| (kc, v.pop().unwrap()))
        .sorted_by(|a, b| a.1.cmp(&b.1))
        // .sorted_by(|a, b| a.1.cmp(b.1))
        .partition(|(_, action_opt)| action_opt.is_recognized());

    // let mut last_discriminant = None;
    let single_action_binds: Vec<_> = single_action_binds.into_iter()
        // .sorted_by(|a,b| {
        //     match (a.1, b.1) {
        //         (ActionOption::Recognized(a), ActionOption::Recognized(b)) => a.meow_ordering(b),
        //         (ActionOption::Unrecognized(_), _) => unreachable!(),
        //         (_,ActionOption::Unrecognized(_)) => unreachable!(),
        //     }
        // })
        .flat_map(|(kc, action_opt)| {
            assert!(matches!(&action_opt, ActionOption::Recognized(_)));

            let key_combo = kc.to_string();

            let mut lines = Vec::new();

            let line = line![
                span!(key_combo_style(&action_opt); "{key_combo:width$} - ", width = max_keycombo_length),
                span!(action_style(&action_opt);"{action_opt}")
            ];

            // if action.discriminant() != last_discriminant {
            //     lines.push(Line::default());
            // }

            // if let Action::BuiltinAction(_) = &action {
            //     if let Some(last) = lines.last() {
            //         let action_span = &line.spans[1];

            //     }
            // }
            max_line_length = max_line_length.max(line.width());

            // line
            lines.push(line);


            // match action_opt {

            //     ActionOption::Unrecognized(unrec) => {
            //         let line = line![
            //             span!(key_combo_style(action_opt); "{key_combo:width$} - ", width = max_keycombo_length),
            //             span!(action_style(action_opt);"{action_opt}")
            //         ];
            //         lines.push(line);
            //     }

            // }


            // last_discriminant = action.discriminant();
            lines
        })
        .collect()
        // .partition(|l| !l.spans.iter().any(|s|s.style == Style::new().yellow()))
    ;

    let unknowns: Vec<Line<'static>> = unknown_single_actions
        .into_iter()
        .map(|(key_combo, action_opt)| {
            let line = line![
                span!(key_combo_style(&action_opt); "{key_combo:width$} - ", width = max_keycombo_length),
                span!(action_style(&action_opt); "{action_opt}")
            ];
            max_line_length = max_line_length.max(line.width());
            line
        })
        .collect();

    if !unknowns.is_empty() {
        rows.push(
            Line::raw("Unrecognized Keybinds:")
                .centered()
                .bold()
                .yellow(),
        );
        rows.extend(unknowns);
        rows.push(Line::default());
    }

    rows.extend(single_action_binds);

    if !all_chains.is_empty() {
        rows.push(Line::default());
        rows.push(Line::raw("Action Chain Keybinds:").centered().bold());
        rows.push(Line::default());

        let chain_rows = all_chains.into_iter().flat_map(|(kc, v)| {
            let key_combo = kc.to_string();

            let mut rows = Vec::new();
            let space = " ";
            for action in v {
                let space_amt = if matches!(action, ActionOption::Recognized(Action::Pause(_))) {
                    max_keycombo_length + 1
                } else {
                    max_keycombo_length
                };

                let line = line![
                    format!("{space:space_amt$} - "),
                    span!(action_style(&action); "{action}")
                ];
                max_line_length = max_line_length.max(line.width());

                rows.push(line);
            }

            let any_unrecognized = rows
                .iter()
                .flat_map(|l| l.spans.iter())
                .any(|s| s.style == Style::new().yellow());

            let key_combo_style = if any_unrecognized {
                Style::new().yellow()
            } else {
                Style::new()
            };

            rows.insert(0, Line::styled(key_combo, key_combo_style));

            rows
        });

        rows.extend(chain_rows);
    }

    let area = {
        let mut block_area = area;
        block_area.width = block_area.width.min((max_line_length as u16) + 2);
        block_area.height = block_area.height.min(20).min(rows.len() as u16 + 2);
        block_area.x = area.width.saturating_sub(block_area.width) / 2;
        block_area.y = area.height.saturating_sub(block_area.height) / 2;
        block_area
    };

    frame.render_widget(Clear, area);

    let block = Block::bordered()
        .title_top(" Keybinds ")
        .title_bottom(Span::styled(" Ctrl-R: Reload ", Style::new().dark_gray()))
        .title_alignment(Alignment::Center);
    frame.render_widget(&block, area);

    let inner = block.inner(area);

    let fixed_scroll = (rows.len().saturating_sub(inner.height as usize) as u16).min(*scroll);

    *scroll = fixed_scroll;

    // *scroll = *scroll.min(rows.len().saturating_sub(inner.height as usize) as u16);

    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(Some("↑"))
        .end_symbol(Some("↓"));
    let mut scrollbar_state = ScrollbarState::new(rows.len().saturating_sub(inner.height as usize))
        .position(*scroll as usize);

    let para = Paragraph::new(rows).scroll((*scroll, 0));

    frame.render_widget(para, inner);

    frame.render_stateful_widget(
        scrollbar,
        area.inner(Margin {
            horizontal: 0,
            vertical: 1,
        }),
        &mut scrollbar_state,
    );
}
