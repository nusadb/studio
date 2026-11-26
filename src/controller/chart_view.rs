//! The Chart tab under the editor: draws the showing result set as a bar, line, scatter,
//! histogram or pie chart (see `crate::chart`), exports it as SVG and copies its numbers.

use std::path::PathBuf;

use slint::{ModelRc, SharedString, VecModel};

use super::{AppWindow, Controller};
use crate::chart::{self, Agg, Kind, Options, Order};
use crate::ui::{ChartBar, ChartPoint, ChartSlice, ChartTick};

/// What the user picked on the Chart tab; reset when a new result arrives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ChartChoice {
    kind: String,
    x: Option<usize>,
    y: Option<usize>,
    series: Option<usize>,
    agg: String,
    order: String,
    top: Option<usize>,
}

impl Default for ChartChoice {
    fn default() -> Self {
        Self {
            kind: "bar".to_owned(),
            x: None,
            y: None,
            series: None,
            agg: "sum".to_owned(),
            order: "result".to_owned(),
            top: None,
        }
    }
}

fn ticks(ticks: &[chart::Tick]) -> ModelRc<ChartTick> {
    #[allow(clippy::cast_possible_truncation)]
    let rows: Vec<ChartTick> = ticks
        .iter()
        .map(|t| ChartTick {
            at: t.at as f32,
            label: t.label.as_str().into(),
        })
        .collect();
    ModelRc::new(VecModel::from(rows))
}

fn index(i: Option<usize>) -> i32 {
    i.and_then(|v| i32::try_from(v).ok()).unwrap_or(-1)
}

impl Controller {
    /// The tab was opened: choose sensible columns the first time, then draw.
    pub(super) fn chart_opened(&self, window: &AppWindow) {
        let Some(result) = self.query_result.borrow().clone() else {
            self.chart_draw(window);
            return;
        };
        let mut choice = self.chart.borrow().clone();
        if choice.x.is_none_or(|x| x >= result.columns.len()) {
            // A label column for X and the first number for Y, the usual shape of a report.
            let numeric: Vec<usize> = (0..result.columns.len())
                .filter(|i| chart::is_numeric(&result, *i))
                .collect();
            let label = (0..result.columns.len()).find(|i| !numeric.contains(i));
            choice.x = label.or_else(|| numeric.first().copied()).or(Some(0));
            choice.y = numeric.iter().copied().find(|i| Some(*i) != choice.x);
            choice.kind = if label.is_none() && numeric.len() == 1 {
                "histogram".to_owned()
            } else if label.is_none() {
                "scatter".to_owned()
            } else {
                "bar".to_owned()
            };
            self.chart.replace(choice);
        }
        self.chart_draw(window);
    }

    /// One control changed: `what` names it, `number` / `text` carry its new value.
    pub(super) fn chart_set(&self, window: &AppWindow, what: &str, number: i32, text: &str) {
        {
            let mut c = self.chart.borrow_mut();
            let pick = usize::try_from(number).ok();
            match what {
                "kind" => c.kind = text.to_owned(),
                "x" => c.x = pick,
                "y" => c.y = pick,
                "series" => c.series = pick,
                "agg" => c.agg = text.to_owned(),
                "order" => c.order = text.to_owned(),
                "top" => c.top = pick.filter(|n| *n > 0),
                _ => {},
            }
        }
        self.chart_draw(window);
    }

    /// A new result is showing: its columns differ, so the choice starts over.
    pub(super) fn chart_reset(&self, window: &AppWindow) {
        self.chart.replace(ChartChoice::default());
        if window.get_result_tab() == "chart" {
            self.chart_opened(window);
        } else {
            self.chart_draw(window);
        }
        self.pivot_reset(window);
    }

    fn chart_build(&self) -> Option<chart::Chart> {
        let result = self.query_result.borrow().clone()?;
        let choice = self.chart.borrow().clone();
        let x = choice.x?;
        let kind = Kind::from_id(&choice.kind);
        let agg = if kind == Kind::Scatter {
            Agg::None
        } else {
            Agg::from_id(&choice.agg)
        };
        let options = Options {
            kind,
            x,
            y: choice.y.filter(|_| kind.uses_y()),
            series: choice.series.filter(|_| kind.uses_series()),
            agg,
            order: Order::from_id(&choice.order),
            top: choice.top,
        };
        Some(chart::build(&result, &options))
    }

    pub(super) fn chart_draw(&self, window: &AppWindow) {
        let columns: Vec<SharedString> = self
            .query_result
            .borrow()
            .as_ref()
            .map(|r| r.columns.iter().map(|c| c.as_str().into()).collect())
            .unwrap_or_default();
        window.set_chart_columns(ModelRc::new(VecModel::from(columns)));
        let choice = self.chart.borrow().clone();
        window.set_chart_kind(choice.kind.as_str().into());
        window.set_chart_x(index(choice.x));
        window.set_chart_y(index(choice.y));
        window.set_chart_series(index(choice.series));
        window.set_chart_agg(choice.agg.as_str().into());
        window.set_chart_order(choice.order.as_str().into());
        window.set_chart_top(index(choice.top));
        let built = self.chart_build().unwrap_or_default();
        #[allow(clippy::cast_possible_truncation)]
        let bars: Vec<ChartBar> = built
            .bars
            .iter()
            .map(|b| ChartBar {
                x0: b.x0 as f32,
                x1: b.x1 as f32,
                y0: b.y0 as f32,
                h: b.h as f32,
                series: i32::try_from(b.series).unwrap_or(0),
                label: b.label.as_str().into(),
                value: b.value.as_str().into(),
            })
            .collect();
        #[allow(clippy::cast_possible_truncation)]
        let points: Vec<ChartPoint> = built
            .points
            .iter()
            .map(|p| ChartPoint {
                x: p.x as f32,
                y: p.y as f32,
                label: p.label.as_str().into(),
                series: i32::try_from(p.series).unwrap_or(0),
            })
            .collect();
        let slices: Vec<ChartSlice> = built
            .slices
            .iter()
            .map(|s| ChartSlice {
                path: s.path.as_str().into(),
                label: s.label.as_str().into(),
                value: format!("{}  ({:.1} %)", s.value, s.share * 100.0).into(),
                series: i32::try_from(s.series).unwrap_or(0),
            })
            .collect();
        let legend: Vec<SharedString> = built.legend.iter().map(|l| l.as_str().into()).collect();
        window.set_chart_bars(ModelRc::new(VecModel::from(bars)));
        window.set_chart_points(ModelRc::new(VecModel::from(points)));
        window.set_chart_slices(ModelRc::new(VecModel::from(slices)));
        window.set_chart_legend(ModelRc::new(VecModel::from(legend)));
        window.set_chart_line(if built.joined { "1" } else { "" }.into());
        window.set_chart_x_ticks(ticks(&built.x_ticks));
        window.set_chart_y_ticks(ticks(&built.y_ticks));
        window.set_chart_x_title(built.x_title.as_str().into());
        window.set_chart_y_title(built.y_title.as_str().into());
        window.set_chart_note(built.note.as_str().into());
    }

    fn chart_svg(&self) -> Option<String> {
        let built = self.chart_build()?;
        if built.bars.is_empty() && built.points.is_empty() && built.slices.is_empty() {
            return None;
        }
        let title = if built.y_title.is_empty() {
            built.x_title.clone()
        } else {
            format!("{} by {}", built.y_title, built.x_title)
        };
        Some(chart::svg(&built, &title))
    }

    pub(super) fn chart_copy(&self, window: &AppWindow) {
        if let Some(svg) = self.chart_svg() {
            window.invoke_copy_text(svg.into());
            self.toast(window, "Copied the chart as SVG", false);
        }
    }

    pub(super) fn chart_copy_data(&self, window: &AppWindow) {
        if let Some(built) = self.chart_build()
            && built.table.len() > 1
        {
            window.invoke_copy_text(chart::table_tsv(&built).into());
            self.toast(
                window,
                "Copied the chart's numbers, tab-separated for a spreadsheet",
                false,
            );
        }
    }

    pub(super) fn chart_export(&self, window: &AppWindow) {
        let Some(svg) = self.chart_svg() else {
            return;
        };
        let Some(path) = pick_svg() else {
            return;
        };
        match std::fs::write(&path, svg) {
            Ok(()) => self.toast(window, &format!("Saved {}", path.display()), false),
            Err(e) => self.toast(window, &format!("Could not save the chart: {e}"), true),
        }
    }
}

fn pick_svg() -> Option<PathBuf> {
    #[cfg(debug_assertions)]
    if let Some(dir) = std::env::var_os("NUSADB_STUDIO_EXPORT_DIR") {
        return Some(PathBuf::from(dir).join("chart.svg"));
    }
    rfd::FileDialog::new()
        .set_title("Export chart")
        .set_file_name("chart.svg")
        .add_filter("SVG image", &["svg"])
        .save_file()
}
