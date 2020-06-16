use crate::modes::*;
use app::App;
use crossterm::{
    event::{self, Event as CEvent, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::path::PathBuf;
use std::{
    error::Error,
    io::{stdout, Write},
    sync::mpsc,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use structopt::*;
use tui::{backend::CrosstermBackend, Terminal};

mod app;
mod error;
mod modes;
mod rss;
mod ui;
mod util;

enum Event<I> {
    Input(I),
    Tick,
}

#[derive(Debug, StructOpt)]
pub struct Options {
    /// feed database path
    #[structopt(short, long)]
    database_path: PathBuf,
    /// time in ms between two ticks
    #[structopt(short, long, default_value = "250")]
    tick_rate: u64,
    /// maximum line length for entries
    #[structopt(short, long, default_value = "90")]
    line_length: usize,
}

pub enum IOCommand {
    Break,
    RefreshFeed(i64),
    RefreshAllFeeds(Vec<i64>),
    SubscribeToFeed(String),
}

fn start_async_io(
    app: Arc<Mutex<App>>,
    rx: mpsc::Receiver<IOCommand>,
    database_path: &std::path::PathBuf,
) -> Result<(), crate::error::Error> {
    use IOCommand::*;
    while let Ok(event) = rx.recv() {
        match event {
            Break => break,
            RefreshFeed(feed_id) => {
                let conn = rusqlite::Connection::open(&database_path)?;
                if let Err(e) = crate::rss::refresh_feed(&conn, feed_id) {
                    let mut app = app.lock().unwrap();
                    app.error_flash = Some(e);
                } else {
                    let mut app = app.lock().unwrap();
                    app.update_current_feed_and_entries()?;
                };
            }
            RefreshAllFeeds(feed_ids) => {
                let mut thread_handles = vec![];
                for feed_id in feed_ids {
                    let database_path = database_path.clone();
                    let thread_handle = std::thread::spawn(move || {
                        let conn = rusqlite::Connection::open(&database_path)?;
                        crate::rss::refresh_feed(&conn, feed_id)
                    });

                    thread_handles.push(thread_handle);
                }

                for thread_handle in thread_handles {
                    match thread_handle.join() {
                        Ok(res) => {
                            if let Err(e) = res {
                                let mut app = app.lock().unwrap();
                                app.error_flash = Some(e);
                                // don't `break` here, as we still want to try to
                                // finish the rest of the feeds
                            }
                        }
                        Err(e) => {
                            let mut app = app.lock().unwrap();
                            app.error_flash = Some(e.into());
                        }
                    }
                }

                let mut app = app.lock().unwrap();
                app.update_current_feed_and_entries()?;
            }
            SubscribeToFeed(feed_subscription_input) => {
                let conn = rusqlite::Connection::open(&database_path)?;
                if let Err(e) = crate::rss::subscribe_to_feed(&conn, &feed_subscription_input) {
                    let mut app = app.lock().unwrap();
                    app.error_flash = Some(e);
                } else {
                    match crate::rss::get_feeds(&conn) {
                        Ok(l) => {
                            let feeds = l.into();
                            let mut app = app.lock().unwrap();
                            app.feed_subscription_input = String::new();
                            app.feeds = feeds;
                            app.select_feeds();
                            app.update_current_feed_and_entries()?;
                        }
                        Err(e) => {
                            let mut app = app.lock().unwrap();
                            app.error_flash = Some(e);
                        }
                    };
                }
            }
        }
    }

    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let options: Options = Options::from_args();

    enable_raw_mode()?;

    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout);

    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;

    // Setup input handling
    let (tx, rx) = mpsc::channel();

    let tick_rate = Duration::from_millis(options.tick_rate);
    thread::spawn(move || {
        let mut last_tick = Instant::now();
        loop {
            // poll for tick rate duration, if no events, sent tick event.
            if event::poll(tick_rate - last_tick.elapsed()).unwrap() {
                if let CEvent::Key(key) = event::read().unwrap() {
                    tx.send(Event::Input(key)).unwrap();
                }
            }
            if last_tick.elapsed() >= tick_rate {
                tx.send(Event::Tick).unwrap();
                last_tick = Instant::now();
            }
        }
    });

    let database_path = options.database_path.clone();

    let app = Arc::new(Mutex::new(App::new(options)?));

    let cloned_app = Arc::clone(&app);

    terminal.clear()?;

    let (io_s, io_r) = mpsc::channel();

    // this thread is for async IO
    let io_thread = thread::spawn(move || -> Result<(), crate::error::Error> {
        start_async_io(app, io_r, &database_path)?;
        Ok(())
    });

    // MAIN THREAD IS DRAW THREAD
    loop {
        let mode = {
            let mut app = cloned_app.lock().unwrap();
            terminal.draw(|mut f| ui::draw(&mut f, &mut app))?;
            app.mode
        };
        match mode {
            Mode::Normal => match rx.recv()? {
                Event::Input(event) => match (event.code, event.modifiers) {
                    (KeyCode::Char('q'), _)
                    | (KeyCode::Char('c'), KeyModifiers::CONTROL)
                    | (KeyCode::Esc, _) => {
                        let mut app = cloned_app.lock().unwrap();
                        if app.error_flash.is_some() {
                            app.error_flash = None;
                        } else {
                            disable_raw_mode()?;
                            execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
                            terminal.show_cursor()?;
                            io_s.send(IOCommand::Break)?;
                            break;
                        }
                    }
                    (KeyCode::Char('r'), KeyModifiers::NONE) => {
                        let mut app = cloned_app.lock().unwrap();
                        match &app.selected {
                            Selected::Feeds => {
                                let feed_id = {
                                    let selected_idx = app.feeds.state.selected().unwrap();
                                    app.feeds.items[selected_idx].id
                                };
                                io_s.send(IOCommand::RefreshFeed(feed_id))?;
                            }
                            _ => app.toggle_read()?,
                        }
                    }
                    (KeyCode::Char('x'), KeyModifiers::NONE) => {
                        let feed_ids = {
                            let app = cloned_app.lock().unwrap();
                            crate::rss::get_feeds(&app.conn)?
                                .iter()
                                .map(|feed| feed.id)
                                .collect::<Vec<_>>()
                        };

                        io_s.send(IOCommand::RefreshAllFeeds(feed_ids))?;
                    }
                    (KeyCode::Char(c), KeyModifiers::NONE) => {
                        let mut app = cloned_app.lock().unwrap();
                        app.on_key(c)
                    }
                    (KeyCode::Left, _) => {
                        let mut app = cloned_app.lock().unwrap();
                        app.on_left()?
                    }
                    (KeyCode::Up, _) => {
                        let mut app = cloned_app.lock().unwrap();
                        app.on_up()?
                    }
                    (KeyCode::Right, _) => {
                        let mut app = cloned_app.lock().unwrap();
                        app.on_right()?
                    }
                    (KeyCode::Down, _) => {
                        let mut app = cloned_app.lock().unwrap();
                        app.on_down()?
                    }
                    (KeyCode::Enter, _) => {
                        let mut app = cloned_app.lock().unwrap();
                        app.on_enter()?
                    }
                    _ => {}
                },
                Event::Tick => (),
            },
            Mode::Editing => match rx.recv()? {
                Event::Input(event) => match event.code {
                    KeyCode::Enter => {
                        let feed_subscription_input = {
                            let app = cloned_app.lock().unwrap();
                            app.feed_subscription_input.clone()
                        };
                        io_s.send(IOCommand::SubscribeToFeed(feed_subscription_input))?;
                    }
                    KeyCode::Char(c) => {
                        let mut app = cloned_app.lock().unwrap();
                        app.feed_subscription_input.push(c);
                    }
                    KeyCode::Backspace => {
                        let mut app = cloned_app.lock().unwrap();
                        app.feed_subscription_input.pop();
                    }
                    KeyCode::Esc => {
                        let mut app = cloned_app.lock().unwrap();
                        app.mode = Mode::Normal;
                    }
                    _ => {}
                },
                Event::Tick => (),
            },
        }
    }

    io_thread.join().unwrap()?;

    Ok(())
}
