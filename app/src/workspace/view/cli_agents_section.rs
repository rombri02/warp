//! The "AGENTS" section at the bottom of the vertical tabs panel, listing the CLI agents running
//! in every window and tab.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use warp_core::features::FeatureFlag;
use warp_core::ui::Icon;
use warp_core::ui::color::coloru_with_opacity;
use warp_core::ui::theme::color::internal_colors;
use warp_core::ui::theme::{Fill as ThemeFill, WarpTheme};
use warpui::color::ColorU;
use warpui::elements::{
    Border, ClippedScrollStateHandle, ClippedScrollable, ConstrainedBox, Container, CornerRadius,
    CrossAxisAlignment, Element, Fill as ElementFill, Flex, Hoverable, MainAxisSize,
    MouseStateHandle, Padding, ParentElement, Radius, ScrollbarWidth, Shrinkable, Text,
};
use warpui::fonts::{Properties, Weight};
use warpui::platform::Cursor;
use warpui::prelude::Align;
use warpui::{AppContext, EntityId, SingletonEntity, ViewHandle, WeakViewHandle};

use super::vertical_tabs::{pane_row_background, tab_pane_tints};
use crate::ai::active_agent_views_model::ActiveAgentViewsModel;
use crate::ai::agent_management::AgentNotificationsModel;
use crate::ai::conversation_status_ui::STATUS_ELEMENT_PADDING;
use crate::appearance::Appearance;
use crate::terminal::TerminalView;
use crate::terminal::cli_agent_sessions::{
    CLIAgentSession, CLIAgentSessionStatus, CLIAgentSessionsModel,
};
use crate::ui_components::agent_icon::terminal_view_agent_icon_variant;
use crate::ui_components::icon_with_status::render_icon_with_status;
use crate::workspace::{WorkspaceAction, WorkspaceRegistry};

const ICON_SPACING: f32 = 4.;
const AGENT_ICON_SIZE: f32 = 22.;
/// Pushes the status badge all the way to the agent icon's bottom-right corner.
const AGENT_ICON_OVERLAY_EXTRA_OVERHANG: f32 = 0.05;
const ROW_MIN_HEIGHT: f32 = 42.;
/// Fits the widest label ("BLOCKED"/"WORKING") so pills line up in a column.
const STATUS_PILL_MIN_WIDTH: f32 = 84.;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(super) enum CliAgentSort {
    /// Window, tab, then pane order.
    #[default]
    Grouped,
    /// Most urgent first, see [`CliAgentDisplayStatus::attention_priority`].
    Priority,
}

#[derive(Default)]
pub(super) struct CliAgentsSectionState {
    pub(super) sort: CliAgentSort,
    pub(super) is_collapsed: bool,
    scroll_state: ClippedScrollStateHandle,
    header_mouse_state: MouseStateHandle,
    sort_toggle_mouse_state: MouseStateHandle,
    item_mouse_states: RefCell<HashMap<EntityId, MouseStateHandle>>,
}

/// What the section shows for a CLI agent session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CliAgentDisplayStatus {
    Blocked,
    /// Stopped, and the user hasn't focused its terminal since.
    Done,
    Failed,
    Working,
    /// Stopped and already seen.
    Idle,
    /// The agent has no plugin reporting its state, so only "it's running" is known.
    Running,
}

impl CliAgentDisplayStatus {
    /// `has_known_status` is false when neither a plugin nor screen detection reports the status.
    fn new(status: &CLIAgentSessionStatus, has_known_status: bool, has_unread: bool) -> Self {
        if !has_known_status {
            return Self::Running;
        }
        match status {
            CLIAgentSessionStatus::InProgress => Self::Working,
            CLIAgentSessionStatus::Blocked { .. } => Self::Blocked,
            CLIAgentSessionStatus::Failed { .. } => Self::Failed,
            CLIAgentSessionStatus::Success | CLIAgentSessionStatus::Cancelled if has_unread => {
                Self::Done
            }
            CLIAgentSessionStatus::Success | CLIAgentSessionStatus::Cancelled => Self::Idle,
        }
    }

    fn for_session(
        terminal_view_id: EntityId,
        session: &CLIAgentSession,
        app: &AppContext,
    ) -> Self {
        Self::new(
            &session.status,
            CLIAgentSessionsModel::as_ref(app).has_known_status(terminal_view_id),
            AgentNotificationsModel::as_ref(app)
                .notifications()
                .has_unread_for_terminal_view(terminal_view_id),
        )
    }

    /// Higher sorts first: whatever needs the user comes before work that is still running.
    fn attention_priority(self) -> u8 {
        match self {
            Self::Blocked => 5,
            Self::Failed => 4,
            Self::Done => 3,
            Self::Working => 2,
            Self::Running => 1,
            Self::Idle => 0,
        }
    }

    fn needs_attention(self) -> bool {
        self.attention_priority() >= Self::Done.attention_priority()
    }

    fn label(self) -> &'static str {
        match self {
            Self::Blocked => "BLOCKED",
            Self::Done => "DONE",
            Self::Failed => "FAILED",
            Self::Working => "WORKING",
            Self::Idle => "IDLE",
            Self::Running => "RUNNING",
        }
    }

    fn icon_and_color(self, theme: &WarpTheme) -> (Option<Icon>, ColorU) {
        let neutral = theme.sub_text_color(theme.background()).into_solid();
        match self {
            Self::Blocked => (Some(Icon::StopFilled), theme.ansi_fg_yellow()),
            Self::Done => (Some(Icon::Check), theme.ansi_fg_green()),
            Self::Failed => (Some(Icon::Triangle), theme.ansi_fg_red()),
            Self::Working => (Some(Icon::ClockLoader), theme.ansi_fg_magenta()),
            Self::Idle => (Some(Icon::Check), neutral),
            Self::Running => (None, neutral),
        }
    }
}

/// Terminal views hosting a CLI agent session across all windows, with the tint of the tab hosting
/// each one, ordered per `sort`.
fn collect_cli_agents(
    sort: CliAgentSort,
    app: &AppContext,
) -> Vec<(WeakViewHandle<TerminalView>, Option<ThemeFill>)> {
    let sessions = CLIAgentSessionsModel::as_ref(app);
    let mut workspaces = WorkspaceRegistry::as_ref(app).all_workspaces(app);
    workspaces.sort_by_key(|(window_id, _)| *window_id);

    let mut agents = Vec::new();
    for (_, workspace) in workspaces {
        for tab in &workspace.as_ref(app).tabs {
            let tints = tab_pane_tints(tab, app);
            let pane_group = tab.pane_group.as_ref(app);
            for pane_id in pane_group.terminal_pane_ids() {
                let Some(terminal_view) = pane_group.terminal_view_from_pane_id(pane_id, app)
                else {
                    continue;
                };
                let Some(session) = sessions.session(terminal_view.id()) else {
                    continue;
                };
                let priority = CliAgentDisplayStatus::for_session(terminal_view.id(), session, app)
                    .attention_priority();
                agents.push((
                    priority,
                    terminal_view.downgrade(),
                    tints.get(&pane_id).copied(),
                ));
            }
        }
    }

    match sort {
        CliAgentSort::Grouped => {}
        // Stable, so ties keep their grouped order.
        CliAgentSort::Priority => agents.sort_by_key(|(priority, ..)| std::cmp::Reverse(*priority)),
    }
    agents
        .into_iter()
        .map(|(_, view, tint)| (view, tint))
        .collect()
}

/// `None` when there are no CLI agents running.
pub(super) fn render_cli_agents_section(
    state: &CliAgentsSectionState,
    app: &AppContext,
) -> Option<Box<dyn Element>> {
    if !FeatureFlag::CLIAgentsSidebar.is_enabled() {
        return None;
    }
    let agents = collect_cli_agents(state.sort, app);
    if agents.is_empty() {
        return None;
    }
    let appearance = Appearance::as_ref(app);
    let theme = appearance.theme();

    let mut section = Flex::column()
        .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
        .with_child(render_header(state, appearance));

    if !state.is_collapsed {
        let mut item_mouse_states = state.item_mouse_states.borrow_mut();
        let live_ids: HashSet<_> = agents.iter().map(|(view, _)| view.id()).collect();
        item_mouse_states.retain(|id, _| live_ids.contains(id));

        let sessions = CLIAgentSessionsModel::as_ref(app);
        let focused_terminal_id = ActiveAgentViewsModel::as_ref(app).get_last_focused_terminal_id();
        let mut rows = Flex::column().with_cross_axis_alignment(CrossAxisAlignment::Stretch);
        for (terminal_view, tint) in agents {
            let Some(terminal_view) = terminal_view.upgrade(app) else {
                continue;
            };
            let Some(session) = sessions.session(terminal_view.id()) else {
                continue;
            };
            rows.add_child(render_agent_row(
                AgentRowProps {
                    terminal_view: &terminal_view,
                    session,
                    is_focused: focused_terminal_id == Some(terminal_view.id()),
                    mouse_state: item_mouse_states
                        .entry(terminal_view.id())
                        .or_default()
                        .clone(),
                    tint,
                },
                app,
            ));
        }
        section.add_child(
            Shrinkable::new(
                1.,
                ClippedScrollable::vertical(
                    state.scroll_state.clone(),
                    rows.finish(),
                    ScrollbarWidth::Custom(4.),
                    theme.nonactive_ui_detail().into(),
                    theme.active_ui_detail().into(),
                    ElementFill::None,
                )
                .with_overlayed_scrollbar()
                .finish(),
            )
            .finish(),
        );
    }

    Some(
        Container::new(section.finish())
            .with_border(Border::top(1.).with_border_fill(theme.outline()))
            .finish(),
    )
}

fn render_header(state: &CliAgentsSectionState, appearance: &Appearance) -> Box<dyn Element> {
    let theme = appearance.theme();
    let sub_text = theme.sub_text_color(theme.background());
    let chevron = if state.is_collapsed {
        Icon::ChevronRight
    } else {
        Icon::ChevronDown
    };
    let sort_label = match state.sort {
        CliAgentSort::Grouped => "grouped",
        CliAgentSort::Priority => "priority",
    };
    let font_family = appearance.ui_font_family();

    let sort_toggle = Hoverable::new(state.sort_toggle_mouse_state.clone(), move |hover| {
        let color = if hover.is_hovered() {
            theme.main_text_color(theme.background())
        } else {
            sub_text
        };
        Text::new_inline(sort_label, font_family, 11.)
            .with_color(color.into())
            .finish()
    })
    .with_cursor(Cursor::PointingHand)
    .on_click(|ctx, _, _| {
        ctx.dispatch_typed_action(WorkspaceAction::ToggleCliAgentsSort);
    })
    .finish();

    let row = Flex::row()
        .with_main_axis_size(MainAxisSize::Max)
        .with_cross_axis_alignment(CrossAxisAlignment::Center)
        .with_spacing(4.)
        .with_child(
            ConstrainedBox::new(chevron.to_warpui_icon(sub_text).finish())
                .with_width(12.)
                .with_height(12.)
                .finish(),
        )
        .with_child(
            Shrinkable::new(
                1.,
                Text::new_inline("AGENTS", font_family, 11.)
                    .with_color(sub_text.into())
                    .finish(),
            )
            .finish(),
        )
        .with_child(sort_toggle)
        .finish();

    Hoverable::new(state.header_mouse_state.clone(), move |hover| {
        let mut container = Container::new(row)
            .with_horizontal_padding(12.)
            .with_vertical_padding(10.);
        if hover.is_hovered() {
            container = container.with_background(internal_colors::fg_overlay_1(theme));
        }
        container.finish()
    })
    .with_cursor(Cursor::PointingHand)
    .on_click(|ctx, _, _| {
        ctx.dispatch_typed_action(WorkspaceAction::ToggleCliAgentsSectionCollapsed);
    })
    .finish()
}

/// The project the agent reports, falling back to the terminal's directory name, then the agent.
fn agent_title(
    session: &CLIAgentSession,
    terminal_view: &ViewHandle<TerminalView>,
    app: &AppContext,
) -> String {
    session
        .session_context
        .project
        .clone()
        .or_else(|| {
            terminal_view.as_ref(app).pwd().and_then(|pwd| {
                std::path::Path::new(&pwd)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
        })
        .unwrap_or_else(|| session.agent.display_name().to_owned())
}

struct AgentRowProps<'a> {
    terminal_view: &'a ViewHandle<TerminalView>,
    session: &'a CLIAgentSession,
    is_focused: bool,
    mouse_state: MouseStateHandle,
    /// The color of the tab (or pane) hosting the agent, matching vertical tabs.
    tint: Option<ThemeFill>,
}

fn render_agent_row(props: AgentRowProps<'_>, app: &AppContext) -> Box<dyn Element> {
    let AgentRowProps {
        terminal_view,
        session,
        is_focused,
        mouse_state,
        tint,
    } = props;
    let appearance = Appearance::as_ref(app);
    let theme = appearance.theme();
    let font_family = appearance.ui_font_family();
    let font_size = appearance.ui_font_size();
    let terminal_view_id = terminal_view.id();

    let status = CliAgentDisplayStatus::for_session(terminal_view_id, session, app);
    let (_, status_color) = status.icon_and_color(theme);

    let mut title_row = Flex::row()
        .with_cross_axis_alignment(CrossAxisAlignment::Center)
        .with_spacing(ICON_SPACING);
    if let Some(variant) = terminal_view_agent_icon_variant(terminal_view.as_ref(app), app) {
        title_row.add_child(render_icon_with_status(
            variant,
            AGENT_ICON_SIZE,
            AGENT_ICON_OVERLAY_EXTRA_OVERHANG,
            theme,
            theme.background(),
        ));
    }
    title_row.add_child(
        Shrinkable::new(
            1.0,
            Text::new_inline(
                agent_title(session, terminal_view, app),
                font_family,
                font_size + 2.,
            )
            .with_color(theme.main_text_color(theme.background()).into())
            .finish(),
        )
        .finish(),
    );

    let subtext = [
        Some(session.agent.display_name().to_owned()),
        terminal_view.as_ref(app).current_git_branch(app),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    let subtext_element = Container::new(
        Text::new_inline(subtext, font_family, font_size)
            .with_color(theme.sub_text_color(theme.background()).into())
            .finish(),
    )
    .with_padding_left(font_size + STATUS_ELEMENT_PADDING * 2. + ICON_SPACING)
    .finish();

    let text_column = Flex::column()
        .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
        .with_child(title_row.finish())
        .with_child(subtext_element)
        .finish();
    let row = Flex::row()
        .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
        .with_spacing(8.)
        .with_child(Shrinkable::new(1.0, text_column).finish())
        .with_child(render_status_pill(status, appearance))
        .finish();

    let hoverable = Hoverable::new(mouse_state, move |hover_state| {
        let is_highlighted = is_focused || hover_state.is_hovered();
        let mut container = Container::new(row)
            .with_horizontal_padding(12.)
            .with_vertical_padding(4.);
        if status.needs_attention() {
            container = container
                .with_padding_left(9.)
                .with_border(Border::left(3.).with_border_color(status_color));
        }
        let container = if let Some(tint) = tint {
            container.with_background(
                pane_row_background(Some(tint), is_highlighted, false, false, false, theme)
                    .unwrap_or(tint),
            )
        } else if is_focused {
            container.with_background(theme.surface_overlay_2())
        } else if is_highlighted {
            container.with_background(theme.surface_overlay_1())
        } else {
            container
        };
        container.finish()
    })
    .with_cursor(Cursor::PointingHand)
    .on_click(move |ctx, _, _| {
        ctx.dispatch_typed_action(WorkspaceAction::FocusTerminalViewInWorkspace {
            terminal_view_id,
        });
    });

    ConstrainedBox::new(hoverable.finish())
        .with_min_height(ROW_MIN_HEIGHT)
        .finish()
}

fn render_status_pill(status: CliAgentDisplayStatus, appearance: &Appearance) -> Box<dyn Element> {
    let theme = appearance.theme();
    let (icon, color) = status.icon_and_color(theme);
    let font_size = appearance.ui_font_size() - 1.;
    let mut content = Flex::row()
        .with_cross_axis_alignment(CrossAxisAlignment::Center)
        .with_spacing(4.);
    if let Some(icon) = icon {
        content.add_child(
            ConstrainedBox::new(icon.to_warpui_icon(ThemeFill::Solid(color)).finish())
                .with_width(font_size)
                .with_height(font_size)
                .finish(),
        );
    }
    content.add_child(
        Text::new_inline(status.label(), appearance.ui_font_family(), font_size)
            .with_style(Properties::default().weight(Weight::Bold))
            .with_color(color)
            .finish(),
    );
    // `Align` (unlike a max-size flex) tolerates the unbounded height of the row's first stretch
    // pass, so the pill can stretch to the row's full height inside scrollables.
    let content = Align::new(content.finish()).finish();
    Container::new(
        ConstrainedBox::new(content)
            .with_min_width(STATUS_PILL_MIN_WIDTH)
            .finish(),
    )
    .with_padding(Padding::uniform(3.).with_left(8.).with_right(8.))
    .with_background(ElementFill::Solid(coloru_with_opacity(color, 20)))
    .with_border(Border::all(1.).with_border_color(coloru_with_opacity(color, 60)))
    .with_corner_radius(CornerRadius::with_all(Radius::Pixels(8.)))
    .finish()
}

#[cfg(test)]
#[path = "cli_agents_section_tests.rs"]
mod tests;
