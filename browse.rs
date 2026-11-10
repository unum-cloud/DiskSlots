//! The screens: Places and Datasets side by side, and the Copy, Free and Regroup screens over them.
//!
//! Every action is one bare key: `↑↓` select, `→` and `←` move between the panes, `c` copy, `f`
//! free, `r` regroup, `/` filter, `?` help, `q` quit; a screen that changes a drive waits for `y`,
//! and `Esc` backs out, or stops a running job after the group in flight.
//!
//! One channel carries terminal events and job progress, and the screens redraw after each message,
//! so nothing polls. A job runs on its own thread while the screens stay live.

use std::fmt;
use std::io;
use std::ops::{ControlFlow, Range};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use crate::copy::{CopyPlan, Progress};
use crate::free::FreePlan;
use crate::list::{
    self, dataset_bytes, drive_line, index, slot_bar, slot_range, Medium, Owner, Survey,
    SAFEGRAPHS_SUFFIX,
};
use crate::places::{size, Address, Place, S3};
use crate::regroup::RegroupPlan;
use crate::slots::{slot_extent, SLOT_BYTES};
use crate::Error;

/// A bordered block's left and right edges, or its top and bottom ones.
const BORDER_CELLS: u16 = 2;
/// The summary, status and key lines each take one row.
const LINE_ROWS: u16 = 1;
/// The terminal the screens draw on, which terminal errors name.
const TERMINAL: &str = "/dev/tty";
const DRIVE_LINE_INDENT: &str = "  ";
const SLOT_BAR_LABEL: &str = "slots ";
const BEFORE: &str = "before ";
const AFTER: &str = "after  ";
const SELECTED_MARKER: &str = "› ";
const UNSELECTED_MARKER: &str = "  ";

const HELP: &[&str] = &[
    "↑↓   select a place, or a dataset once → moved there",
    "→ ←  move between Places and Datasets",
    "c    copy the selected dataset to another place",
    "f    free the selected slot run",
    "r    regroup the selected drive's runs towards its start",
    "/    filter datasets by name",
    "Enter  plan a copy to the chosen destination",
    "y    start a copy, free or regroup",
    "Esc  back out, or stop a running job after the group in flight",
    "q    quit",
];

/// Opens the screens over the places at `places`, `~/Data` and the raw NVMe namespaces.
pub fn browse(places: &[Address], endpoint: Option<&str>) -> Result<(), Error> {
    let survey = list::survey(places, endpoint)?;
    let (sender, receiver) = mpsc::channel();
    let mut terminal = ratatui::init();
    let terminal_sender = sender.clone();
    // Detached, as it waits on the terminal until the process ends.
    thread::spawn(move || loop {
        let message = match event::read() {
            Ok(event) => Message::Terminal(event),
            Err(error) => Message::TerminalFailed(error),
        };
        let failed = matches!(message, Message::TerminalFailed(_));
        if terminal_sender.send(message).is_err() || failed {
            return;
        }
    });
    let mut browser = Browser {
        addresses: places,
        endpoint,
        survey,
        focus: Focus::Places,
        place: 0,
        row: 0,
        filter: String::new(),
        screen: Screen::Panes,
        report: Report::Ready,
    };
    let result = browser.run(&mut terminal, &receiver, &sender);
    ratatui::restore();
    result
}

/// What reaches the event loop.
enum Message {
    Terminal(Event),
    TerminalFailed(io::Error),
    Progress(Progress),
    Finished(Result<(), Error>),
}

fn terminal_error(source: io::Error) -> Error {
    Error::Io {
        address: Address::path(Path::new(TERMINAL)),
        source,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Places,
    Datasets,
}

/// A planned change waiting for `y`, and what it runs on.
enum Job {
    Copy { plan: CopyPlan, s3: Option<S3> },
    Free { plan: FreePlan, dataset: u32 },
    Regroup { plan: RegroupPlan, place: u32 },
}

/// What covers the panes, by indices into the survey.
enum Screen {
    Panes,
    Filter,
    Help,
    Copy {
        dataset: u32,
        /// The destination place.
        destination: u32,
        /// The index among the destination's free runs long enough, on a drive.
        run: u32,
    },
    Confirm(Job),
    Running {
        job: Arc<Job>,
        progress: Progress,
        /// Asks the job to stop after the group in flight.
        stop: Sender<()>,
    },
}

/// The line under the panes: how the last job ended, or why a key did nothing.
enum Report {
    Ready,
    Done(Arc<Job>),
    /// A job that stopped, or a change refused before it started; the error names its address.
    Failed(Error),
    Note(&'static str),
}

struct Browser<'arguments> {
    addresses: &'arguments [Address],
    endpoint: Option<&'arguments str>,
    survey: Survey,
    focus: Focus,
    place: u32,
    /// The selected row among the place's datasets the filter lets through.
    row: u32,
    /// Lowercase.
    filter: String,
    screen: Screen,
    report: Report,
}

impl Browser<'_> {
    fn run(
        &mut self,
        terminal: &mut DefaultTerminal,
        receiver: &Receiver<Message>,
        sender: &Sender<Message>,
    ) -> Result<(), Error> {
        loop {
            terminal
                .draw(|frame| self.draw(frame))
                .map_err(terminal_error)?;
            match receiver.recv().expect("the event loop holds a sender") {
                Message::Terminal(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    if self.key(key.code, sender).is_break() {
                        return Ok(());
                    }
                }
                Message::Terminal(_) => {}
                Message::TerminalFailed(source) => return Err(terminal_error(source)),
                Message::Progress(progress) => {
                    if let Screen::Running {
                        progress: shown, ..
                    } = &mut self.screen
                    {
                        *shown = progress;
                    }
                }
                Message::Finished(result) => {
                    let Screen::Running { job, .. } =
                        std::mem::replace(&mut self.screen, Screen::Panes)
                    else {
                        unreachable!("only a running job finishes");
                    };
                    self.report = match result {
                        Ok(()) => Report::Done(job),
                        Err(error) => Report::Failed(error),
                    };
                    self.survey = list::survey(self.addresses, self.endpoint)?;
                    self.place = self
                        .place
                        .min(index(self.survey.places.len()).saturating_sub(1));
                    self.row = 0;
                }
            }
        }
    }

    /// The selected place's datasets the filter lets through.
    fn visible(&self) -> impl Iterator<Item = u32> + Clone + '_ {
        self.survey.datasets_of(self.place).filter(|&dataset| {
            self.filter.is_empty()
                || self
                    .survey
                    .dataset_name(dataset)
                    .to_lowercase()
                    .contains(&self.filter)
        })
    }

    /// Handles one key press, breaking to quit.
    fn key(&mut self, code: KeyCode, sender: &Sender<Message>) -> ControlFlow<()> {
        let places = index(self.survey.places.len());
        match (&mut self.screen, code) {
            (Screen::Panes, _) => return self.panes_key(code),
            (Screen::Help, _) => self.screen = Screen::Panes,
            (Screen::Filter, KeyCode::Char(character)) => {
                self.filter.extend(character.to_lowercase());
                self.row = 0;
            }
            (Screen::Filter, KeyCode::Backspace) => {
                self.filter.pop();
                self.row = 0;
            }
            (Screen::Filter, KeyCode::Esc) => {
                self.filter.clear();
                self.screen = Screen::Panes;
            }
            (Screen::Filter, KeyCode::Enter) => self.screen = Screen::Panes,
            (Screen::Running { stop, .. }, KeyCode::Esc) => {
                // The job may have ended already, and then nobody listens.
                let _ = stop.send(());
            }
            (Screen::Copy { .. } | Screen::Confirm(_), KeyCode::Esc) => self.screen = Screen::Panes,
            (Screen::Confirm(_), KeyCode::Char('y')) => {
                let Screen::Confirm(job) = std::mem::replace(&mut self.screen, Screen::Panes)
                else {
                    unreachable!("the screen was matched as a confirmation");
                };
                self.start(job, sender);
            }
            (
                Screen::Copy {
                    dataset,
                    destination,
                    run,
                },
                code,
            ) => {
                let (dataset, runs) = (*dataset, run_count(&self.survey, *destination, *dataset));
                match code {
                    KeyCode::Up | KeyCode::Down => {
                        let stride = if code == KeyCode::Down { 1 } else { places - 1 };
                        if let Some(next) =
                            next_destination(&self.survey, dataset, *destination, stride)
                        {
                            (*destination, *run) = (next, 0);
                        }
                    }
                    KeyCode::Left => *run = run.saturating_sub(1),
                    KeyCode::Right => *run = (*run + 1).min(runs.saturating_sub(1)),
                    KeyCode::Enter if runs > 0 || !is_drive(&self.survey, *destination) => {
                        let (destination, run) = (*destination, *run);
                        self.plan_copy(dataset, destination, run);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }

    fn panes_key(&mut self, code: KeyCode) -> ControlFlow<()> {
        self.report = Report::Ready;
        let last_place = index(self.survey.places.len()).saturating_sub(1);
        let last_row = index(self.visible().count()).saturating_sub(1);
        let selected = self.visible().nth(self.row as usize);
        match (code, self.focus) {
            (KeyCode::Char('q'), _) => return ControlFlow::Break(()),
            (KeyCode::Up, Focus::Places) => {
                (self.place, self.row) = (self.place.saturating_sub(1), 0)
            }
            (KeyCode::Down, Focus::Places) => {
                (self.place, self.row) = ((self.place + 1).min(last_place), 0)
            }
            (KeyCode::Up, Focus::Datasets) => self.row = self.row.saturating_sub(1),
            (KeyCode::Down, Focus::Datasets) => self.row = (self.row + 1).min(last_row),
            (KeyCode::Right | KeyCode::Enter, Focus::Places) => self.focus = Focus::Datasets,
            (KeyCode::Left, Focus::Datasets) => self.focus = Focus::Places,
            (KeyCode::Char('/'), _) => self.screen = Screen::Filter,
            (KeyCode::Char('?'), _) => self.screen = Screen::Help,
            (KeyCode::Char('c'), Focus::Datasets) => {
                let Some(dataset) = selected else {
                    return ControlFlow::Continue(());
                };
                let from = self.survey.dataset(dataset).place;
                match next_destination(&self.survey, dataset, from, 1) {
                    Some(destination) => {
                        self.screen = Screen::Copy {
                            dataset,
                            destination,
                            run: 0,
                        }
                    }
                    None => {
                        self.report = Report::Note("no other place takes a copy of this dataset")
                    }
                }
            }
            (KeyCode::Char('f'), Focus::Datasets) => {
                let freeable = selected.filter(|&dataset| {
                    let found = self.survey.dataset(dataset);
                    is_drive(&self.survey, found.place)
                        && matches!(
                            found.owner,
                            Owner::SafeGraphs(_) | Owner::UStore { .. } | Owner::Directory { .. }
                        )
                });
                let Some(dataset) = freeable else {
                    self.report =
                        Report::Note("f frees a SafeGraphs or UStore run on a drive; pick one");
                    return ControlFlow::Continue(());
                };
                match FreePlan::new(&self.survey.dataset_address(dataset)) {
                    Ok(plan) => self.screen = Screen::Confirm(Job::Free { plan, dataset }),
                    Err(error) => self.report = Report::Failed(error),
                }
            }
            (KeyCode::Char('r'), _) => {
                let Place::Drive(drive) = &self.survey.places[self.place as usize] else {
                    self.report = Report::Note("r regroups a drive; pick one");
                    return ControlFlow::Continue(());
                };
                match RegroupPlan::new(drive) {
                    Ok(plan) => {
                        self.screen = Screen::Confirm(Job::Regroup {
                            plan,
                            place: self.place,
                        })
                    }
                    Err(error) => self.report = Report::Failed(error),
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }

    /// Plans copying `dataset` to `destination`, checking every rule before `y` can start it.
    fn plan_copy(&mut self, dataset: u32, destination: u32, run: u32) {
        let source = self.survey.dataset_address(dataset);
        let target = copy_destination(&self.survey, dataset, destination, run);
        let planned = S3::for_addresses([&source, &target], self.endpoint)
            .and_then(|s3| Ok((CopyPlan::new(&source, &target, s3.as_ref())?, s3)));
        match planned {
            Ok((plan, s3)) => self.screen = Screen::Confirm(Job::Copy { plan, s3 }),
            Err(error) => self.report = Report::Failed(error),
        }
    }

    /// Runs `job` on a worker thread, which reports progress and its end through the channel and
    /// stops after the group in flight once `Esc` asks it to.
    fn start(&mut self, job: Job, sender: &Sender<Message>) {
        let job = Arc::new(job);
        let (worker_job, worker_sender) = (Arc::clone(&job), sender.clone());
        let (stop, stopped) = mpsc::channel();
        thread::spawn(move || {
            let result = worker_job.run(|progress| {
                // The loop may have ended on a terminal failure, and then nobody draws progress.
                let _ = worker_sender.send(Message::Progress(*progress));
                match stopped.try_recv() {
                    Ok(()) => ControlFlow::Break(()),
                    Err(_) => ControlFlow::Continue(()),
                }
            });
            let _ = worker_sender.send(Message::Finished(result));
        });
        let progress = Progress::new(job.bytes());
        self.screen = Screen::Running {
            job,
            progress,
            stop,
        };
    }

    fn draw(&self, frame: &mut Frame) {
        let [header, body, status, keys] = Layout::vertical([
            Constraint::Length(LINE_ROWS),
            Constraint::Min(0),
            Constraint::Length(LINE_ROWS),
            Constraint::Length(LINE_ROWS),
        ])
        .areas(frame.area());
        let bold = Style::new().add_modifier(Modifier::BOLD);
        frame.render_widget(Paragraph::new(self.survey.summary()).style(bold), header);
        let places: Vec<Vec<String>> = (0..index(self.survey.places.len()))
            .map(|place| self.place_lines(place))
            .collect();
        let widest = places
            .iter()
            .flatten()
            .map(|line| line.chars().count())
            .max()
            .unwrap_or_default();
        let width = u16::try_from(widest)
            .unwrap_or(u16::MAX)
            .saturating_add(BORDER_CELLS);
        let [places_area, datasets] =
            Layout::horizontal([Constraint::Length(width), Constraint::Min(0)]).areas(body);
        let items: Vec<ListItem> = places
            .into_iter()
            .map(|lines| ListItem::new(lines.into_iter().map(Line::from).collect::<Vec<_>>()))
            .collect();
        let mut state = ListState::default().with_selected(Some(self.place as usize));
        let list = List::new(items).block(self.pane(" Places ".into(), Focus::Places));
        frame.render_stateful_widget(
            list.highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
            places_area,
            &mut state,
        );
        self.draw_datasets(frame, datasets);
        let report = match &self.report {
            Report::Ready => String::new(),
            Report::Done(job) => format!("done: {job}"),
            Report::Failed(error) => error.to_string(),
            Report::Note(note) => (*note).to_owned(),
        };
        frame.render_widget(Paragraph::new(report), status);
        let (title, footer) = match &self.screen {
            Screen::Panes => (
                "",
                "↑↓ select   →← panes   c copy   f free   r regroup   / filter   ? help   q quit",
            ),
            Screen::Filter => (" Filter ", "type to filter   Enter keep   Esc clear"),
            Screen::Help => (" Help ", "any key returns"),
            Screen::Copy { .. } => (
                " Copy ",
                "↑↓ destination   ←→ free run   Enter plan   Esc back",
            ),
            Screen::Confirm(Job::Copy { .. }) => (" Copy ", "y start   Esc back"),
            Screen::Confirm(Job::Free { .. }) => (" Free ", "y free   Esc back"),
            Screen::Confirm(Job::Regroup { .. }) => (" Regroup ", "y regroup   Esc back"),
            Screen::Running { .. } => (" Running ", "Esc stops after the group in flight"),
        };
        frame.render_widget(Paragraph::new(footer), keys);
        let lines = self.overlay_lines(body.width.saturating_sub(BORDER_CELLS));
        if !lines.is_empty() {
            draw_overlay(frame, body, title, lines);
        }
    }

    /// A place's line, and a drive's health under it.
    fn place_lines(&self, place: u32) -> Vec<String> {
        let mut lines = vec![self.survey.place_line(place)];
        if let Medium::Drive(drive) = &self.survey.media[place as usize] {
            lines.push(format!("{DRIVE_LINE_INDENT}{}", drive_line(drive)));
        }
        lines
    }

    fn draw_datasets(&self, frame: &mut Frame, area: Rect) {
        let Some(location) = self.survey.places.get(self.place as usize) else {
            frame.render_widget(self.pane(" Datasets ".into(), Focus::Datasets), area);
            return;
        };
        let block = self.pane(format!(" Datasets in {location} "), Focus::Datasets);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let drive = match &self.survey.media[self.place as usize] {
            Medium::Drive(drive) => Some(drive),
            _ => None,
        };
        let bar_rows = if drive.is_some() { LINE_ROWS } else { 0 };
        let [rows, bar] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(bar_rows)]).areas(inner);
        let items: Vec<ListItem> = self
            .survey
            .dataset_rows(self.visible())
            .into_iter()
            .map(ListItem::new)
            .collect();
        let selected = (self.focus == Focus::Datasets).then_some(self.row as usize);
        let mut state = ListState::default().with_selected(selected);
        let list = List::new(items).highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        frame.render_stateful_widget(list, rows, &mut state);
        if let Some(drive) = drive {
            let columns = u32::from(bar.width).saturating_sub(index(SLOT_BAR_LABEL.len()));
            let line = slot_bar(drive.slots, columns, self.drive_runs(self.place));
            frame.render_widget(Paragraph::new(format!("{SLOT_BAR_LABEL}{line}")), bar);
        }
    }

    /// The slots each dataset on drive `place` covers, and its mark.
    fn drive_runs(&self, place: u32) -> impl Iterator<Item = (Range<u32>, char)> + Clone + '_ {
        self.survey.datasets_of(place).map(|dataset| {
            let found = self.survey.dataset(dataset);
            (slot_range(&found.extent), found.owner.mark())
        })
    }

    fn pane(&self, title: String, focus: Focus) -> Block<'static> {
        let block = Block::bordered().title(title);
        match self.focus == focus && matches!(self.screen, Screen::Panes) {
            true => block.border_style(Style::new().add_modifier(Modifier::BOLD)),
            false => block,
        }
    }

    /// The overlay's lines, `width` columns at most, empty over the bare panes.
    fn overlay_lines(&self, width: u16) -> Vec<String> {
        let survey = &self.survey;
        match &self.screen {
            Screen::Panes => Vec::new(),
            Screen::Filter => vec![format!("/{}", self.filter)],
            Screen::Help => HELP.iter().map(|line| (*line).to_owned()).collect(),
            Screen::Copy {
                dataset,
                destination,
                run,
            } => self.copy_lines(*dataset, *destination, *run),
            Screen::Confirm(Job::Copy { plan, .. }) => vec![plan.to_string()],
            Screen::Confirm(Job::Free { plan, dataset }) => {
                let (name, bytes) = (
                    survey.dataset_name(*dataset),
                    dataset_bytes(survey.dataset(*dataset)),
                );
                vec![format!(
                    "{plan}, {name} {}, {}",
                    size(bytes),
                    survey.copies_line(*dataset)
                )]
            }
            Screen::Confirm(Job::Regroup { plan, place }) => {
                self.regroup_lines(*place, plan, width)
            }
            Screen::Running { job, progress, .. } => vec![job.to_string(), progress.to_string()],
        }
    }

    fn copy_lines(&self, dataset: u32, destination: u32, run: u32) -> Vec<String> {
        let survey = &self.survey;
        let found = survey.dataset(dataset);
        let (name, owner, bytes) = (
            survey.dataset_name(dataset),
            found.owner.name(),
            size(dataset_bytes(found)),
        );
        let mut lines = vec![format!(
            "{name} {owner}, {bytes} from {}",
            survey.places[found.place as usize]
        )];
        for place in
            (0..index(survey.places.len())).filter(|&place| takes_copy(survey, dataset, place))
        {
            let marker = if place == destination {
                SELECTED_MARKER
            } else {
                UNSELECTED_MARKER
            };
            let mut line = format!("{marker}{}", survey.places[place as usize]);
            if place == destination && is_drive(survey, place) {
                match runs_for(survey, place, dataset).nth(run as usize) {
                    Some((first, _)) => {
                        let runs = run_count(survey, place, dataset);
                        line += &format!("   free run at slot {first}, {} of {runs}", run + 1);
                    }
                    None => line += "   no free run is long enough",
                }
            }
            lines.push(line);
        }
        lines
    }

    fn regroup_lines(&self, place: u32, plan: &RegroupPlan, width: u16) -> Vec<String> {
        let Medium::Drive(drive) = &self.survey.media[place as usize] else {
            return Vec::new();
        };
        let columns = u32::from(width).saturating_sub(index(BEFORE.len()));
        let moved = |(run, mark): (Range<u32>, char)| match plan
            .moves
            .iter()
            .find(|step| step.from_slot == run.start)
        {
            Some(step) => (step.to_slot..step.to_slot + run.len() as u32, mark),
            None => (run, mark),
        };
        let mut lines = vec![
            format!(
                "{BEFORE}{}",
                slot_bar(drive.slots, columns, self.drive_runs(place))
            ),
            format!(
                "{AFTER}{}",
                slot_bar(drive.slots, columns, self.drive_runs(place).map(moved))
            ),
        ];
        for step in &plan.moves {
            let name = self
                .survey
                .datasets_of(place)
                .find(|&dataset| {
                    slot_range(&self.survey.dataset(dataset).extent).start == step.from_slot
                })
                .map(|dataset| self.survey.dataset_name(dataset))
                .unwrap_or_default();
            lines.push(format!("move {name}: {step}"));
        }
        if plan.moves.is_empty() {
            lines.push("nothing to move".to_owned());
        }
        lines
    }
}

impl Job {
    fn run(&self, report: impl FnMut(&Progress) -> ControlFlow<()>) -> Result<(), Error> {
        match self {
            Job::Copy { plan, s3 } => plan.run(s3.as_ref(), report),
            Job::Free { plan, .. } => plan.run(report),
            Job::Regroup { plan, .. } => plan.run(report),
        }
    }

    /// The bytes it moves, or frees.
    fn bytes(&self) -> u64 {
        match self {
            Job::Copy { plan, .. } => plan.bytes,
            Job::Free { .. } | Job::Regroup { .. } => 0,
        }
    }
}

impl fmt::Display for Job {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Job::Copy { plan, .. } => plan.fmt(formatter),
            Job::Free { plan, .. } => plan.fmt(formatter),
            Job::Regroup { plan, .. } => plan.fmt(formatter),
        }
    }
}

/// A bordered box sized to its lines, centered over `area`.
fn draw_overlay(frame: &mut Frame, area: Rect, title: &str, lines: Vec<String>) {
    let widest = lines
        .iter()
        .map(String::as_str)
        .chain([title])
        .map(|line| line.chars().count())
        .max();
    let fit = |cells: usize, limit: u16| {
        u16::try_from(cells)
            .unwrap_or(u16::MAX)
            .saturating_add(BORDER_CELLS)
            .min(limit)
    };
    let (width, height) = (
        fit(widest.unwrap_or_default(), area.width),
        fit(lines.len(), area.height),
    );
    let [column] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(area);
    let [overlay] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(column);
    frame.render_widget(Clear, overlay);
    let text: Vec<Line> = lines.into_iter().map(Line::from).collect();
    frame.render_widget(
        Paragraph::new(text).block(Block::bordered().title(title)),
        overlay,
    );
}

fn is_drive(survey: &Survey, place: u32) -> bool {
    matches!(survey.media[place as usize], Medium::Drive(_))
}

/// Whether `place` may take a copy of `dataset`: a folder, a bucket, or a drive for a SafeGraphs
/// file, other than the dataset's own place.
fn takes_copy(survey: &Survey, dataset: u32, place: u32) -> bool {
    let found = survey.dataset(dataset);
    place != found.place
        && match (
            &survey.places[place as usize],
            &survey.media[place as usize],
        ) {
            (_, Medium::Unreachable(_)) => false,
            (Place::Folder(_) | Place::Bucket(_), _) => true,
            (Place::Drive(_), _) => matches!(found.owner, Owner::SafeGraphs(_)),
            (Place::File(_) | Place::Bound(_), _) => false,
        }
}

/// The next place `stride` places on from `from`, wrapping around, that takes a copy of `dataset`.
fn next_destination(survey: &Survey, dataset: u32, from: u32, stride: u32) -> Option<u32> {
    let places = index(survey.places.len());
    (1..places)
        .map(|step| (from + step * stride) % places)
        .find(|&place| takes_copy(survey, dataset, place))
}

/// The free runs on drive `place` that hold `dataset`, as `(first slot, slots)`.
fn runs_for(survey: &Survey, place: u32, dataset: u32) -> impl Iterator<Item = (u32, u32)> {
    let needed = slots_needed(survey, dataset);
    let mut runs = Vec::new();
    if let Medium::Drive(drive) = &survey.media[place as usize] {
        let mut start = 0;
        for occupied in survey
            .datasets_of(place)
            .map(|dataset| slot_range(&survey.dataset(dataset).extent))
        {
            if occupied.start > start {
                runs.push((start, occupied.start - start));
            }
            start = start.max(occupied.end);
        }
        if start < drive.slots {
            runs.push((start, drive.slots - start));
        }
    }
    runs.into_iter().filter(move |&(_, slots)| slots >= needed)
}

fn run_count(survey: &Survey, place: u32, dataset: u32) -> u32 {
    index(runs_for(survey, place, dataset).count())
}

fn slots_needed(survey: &Survey, dataset: u32) -> u32 {
    let bytes = dataset_bytes(survey.dataset(dataset));
    u32::try_from(bytes.div_ceil(SLOT_BYTES).max(1)).unwrap_or(u32::MAX)
}

/// Where a copy of `dataset` lands: the chosen free run on a drive, else the place itself, with a
/// file name for a slot run, which carries none of its own.
fn copy_destination(survey: &Survey, dataset: u32, destination: u32, run: u32) -> Address {
    if let Some((first, _)) = runs_for(survey, destination, dataset).nth(run as usize) {
        return survey.address(
            destination,
            slot_extent(first, slots_needed(survey, dataset)),
        );
    }
    let address = survey.address(destination, 0..0);
    let found = survey.dataset(dataset);
    if !found.name.is_empty() || !is_drive(survey, found.place) {
        return address;
    }
    let name = format!("{}{SAFEGRAPHS_SUFFIX}", survey.dataset_name(dataset));
    Address {
        location: address.location.join(&name),
        ..address
    }
}
