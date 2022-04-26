//! Implements the glue between OS input/output and keyberon state management.

use anyhow::{bail, Result};
use log::{error, info};

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use std::collections::HashSet;
use std::convert::TryFrom;
use std::path::PathBuf;
use std::time;

use parking_lot::Mutex;
use std::sync::Arc;

use crate::cfg;
use crate::custom_action::*;
use crate::keys::*;
use crate::oskbd::*;

use keyberon::key_code::*;
use keyberon::layout::*;

pub struct Kanata {
    pub kbd_in_path: PathBuf,
    pub kbd_out: KbdOut,
    pub mapped_keys: [bool; cfg::MAPPED_KEYS_LEN],
    pub key_outputs: cfg::KeyOutputs,
    pub layout: cfg::KanataLayout,
    pub prev_keys: Vec<KeyCode>,
    last_tick: time::Instant,
}

#[cfg(target_os = "windows")]
use once_cell::sync::Lazy;
#[cfg(target_os = "windows")]
static PRESSED_KEYS: Lazy<Mutex<HashSet<OsCode>>> = Lazy::new(|| Mutex::new(HashSet::new()));

impl Kanata {
    /// Create a new configuration from a file.
    pub fn new(cfg: PathBuf) -> Result<Self> {
        let cfg = cfg::Cfg::new_from_file(&cfg)?;

        let kbd_out = match KbdOut::new() {
            Ok(kbd_out) => kbd_out,
            Err(err) => {
                error!("Failed to open the output uinput device. Make sure you've added kanata to the `uinput` group");
                bail!(err)
            }
        };

        #[cfg(target_os = "linux")]
        let kbd_in_path = cfg
            .items
            .get("linux-dev")
            .expect("linux-dev required in defcfg")
            .into();
        #[cfg(target_os = "windows")]
        let kbd_in_path = "unused".into();

        Ok(Self {
            kbd_in_path,
            kbd_out,
            mapped_keys: cfg.mapped_keys,
            key_outputs: cfg.key_outputs,
            layout: cfg.layout,
            prev_keys: Vec::new(),
            last_tick: time::Instant::now(),
        })
    }

    /// Create a new configuration from a file, wrapped in an Arc<Mutex<_>>
    pub fn new_arc(cfg: PathBuf) -> Result<Arc<Mutex<Self>>> {
        Ok(Arc::new(Mutex::new(Self::new(cfg)?)))
    }

    /// Update keyberon layout state for press/release, handle repeat separately
    fn handle_key_event(&mut self, event: &KeyEvent) -> Result<()> {
        let evc: u32 = event.code.into();
        let kbrn_ev = match event.value {
            KeyValue::Press => Event::Press(0, evc as u8),
            KeyValue::Release => Event::Release(0, evc as u8),
            KeyValue::Repeat => return self.handle_repeat(event),
        };
        self.layout.event(kbrn_ev);
        Ok(())
    }

    /// Advance keyberon layout state and send events based on changes to its state.
    fn handle_time_ticks(&mut self) -> Result<()> {
        let now = time::Instant::now();
        let ms_elapsed = now.duration_since(self.last_tick).as_millis();

        if ms_elapsed > 0 {
            self.last_tick = now;
        }

        for _ in 0..ms_elapsed {
            // Only send on the press. No repeat action is supported for this for the time being.
            if let CustomEvent::Press(custact) = self.layout.tick() {
                match custact {
                    CustomAction::Unicode(c) => self.kbd_out.send_unicode(*c)?,
                }
            }

            let cur_keys: Vec<KeyCode> = self.layout.keycodes().collect();

            // Release keys that are missing from the current state but exist in the previous
            // state. It's important to iterate using a Vec because the order matters. This used to
            // use HashSet force computing `difference` but that iteration order is random which is
            // not what we want.
            for k in &self.prev_keys {
                if cur_keys.contains(k) {
                    continue;
                }
                log::debug!("release   {:?}", k);
                if let Err(e) = self.kbd_out.release_key(k.into()) {
                    bail!("failed to release key: {:?}", e);
                }
            }
            // Press keys that exist in the current state but are missing from the previous state.
            // Comment above regarding Vec/HashSet also applies here.
            for k in &cur_keys {
                if self.prev_keys.contains(k) {
                    continue;
                }
                log::debug!("press     {:?}", k);
                if let Err(e) = self.kbd_out.press_key(k.into()) {
                    bail!("failed to press key: {:?}", e);
                }
            }
            self.prev_keys = cur_keys;
        }
        Ok(())
    }

    /// This compares the active keys in the keyberon layout against the potential key outputs for
    /// corresponding physical key in the configuration. If any of keyberon active keys match any
    /// potential physical key output, write the repeat event to the OS.
    fn handle_repeat(&mut self, event: &KeyEvent) -> Result<()> {
        let active_keycodes: HashSet<KeyCode> = self.layout.keycodes().collect();
        let idx: usize = event.code.into();
        let outputs_for_key: &Vec<OsCode> = match &self.key_outputs[idx] {
            None => return Ok(()),
            Some(v) => v,
        };
        let mut output = None;
        for valid_output in outputs_for_key.iter() {
            if active_keycodes.contains(&valid_output.into()) {
                output = Some(valid_output);
                break;
            }
        }
        if let Some(kc) = output {
            log::debug!("repeat    {:?}", KeyCode::from(*kc));
            if let Err(e) = self.kbd_out.write_key(*kc, KeyValue::Repeat) {
                bail!("could not write key {:?}", e)
            }
        }
        Ok(())
    }

    /// Starts a new thread that processes OS key events and advances the keyberon layout's state.
    pub fn start_processing_loop(kanata: Arc<Mutex<Self>>, rx: Receiver<KeyEvent>) {
        info!("Kanata: entering the processing loop");
        std::thread::spawn(move || {
            // This is done to try and work around a weird issue where upon starting kanata, it seems
            // that enter is being held constantly until any new keycode is sent.
            #[cfg(target_os = "linux")]
            {
                info!("Sending press+release for space repeatedly");
                for _ in 0..1000 {
                    let mut kanata = kanata.lock();
                    kanata.kbd_out.press_key(OsCode::KEY_SPACE).unwrap();
                    kanata.kbd_out.release_key(OsCode::KEY_SPACE).unwrap();
                    std::thread::sleep(time::Duration::from_millis(1));
                }
            }

            info!("Starting processing loop");
            let err = loop {
                match rx.try_recv() {
                    Ok(kev) => {
                        let mut k = kanata.lock();
                        if let Err(e) = k.handle_key_event(&kev) {
                            break e;
                        }
                        if let Err(e) = k.handle_time_ticks() {
                            break e;
                        }
                    }
                    Err(TryRecvError::Empty) => {
                        if let Err(e) = kanata.lock().handle_time_ticks() {
                            break e;
                        }
                        std::thread::sleep(time::Duration::from_millis(1));
                    }
                    Err(TryRecvError::Disconnected) => {
                        panic!("channel disconnected")
                    }
                }
            };
            panic!("processing loop encountered error {:?}", err)
        });
    }

    /// Enter an infinite loop that listens for OS key events and sends them to the processing
    /// thread.
    #[cfg(target_os = "linux")]
    pub fn event_loop(kanata: Arc<Mutex<Self>>, tx: Sender<KeyEvent>) -> Result<()> {
        info!("Kanata: entering the event loop");

        let (kbd_in, mapped_keys) = {
            let kanata = kanata.lock();
            let kbd_in = match KbdIn::new(&kanata.kbd_in_path) {
                Ok(kbd_in) => kbd_in,
                Err(e) => {
                    bail!("failed to open keyboard device: {}", e)
                }
            };
            (kbd_in, kanata.mapped_keys)
        };

        loop {
            let in_event = kbd_in.read()?;

            // Pass-through non-key events
            let key_event = match KeyEvent::try_from(in_event.clone()) {
                Ok(ev) => ev,
                _ => {
                    let mut kanata = kanata.lock();
                    kanata.kbd_out.write(in_event)?;
                    continue;
                }
            };

            // Check if this keycode is mapped in the configuration. If it hasn't been mapped, send
            // it immediately.
            let kc: usize = key_event.code.into();
            if kc >= cfg::MAPPED_KEYS_LEN || !mapped_keys[kc] {
                let mut kanata = kanata.lock();
                kanata.kbd_out.write_key(key_event.code, key_event.value)?;
                continue;
            }

            // Send key events to the processing loop
            if let Err(e) = tx.send(key_event) {
                bail!("failed to send on channel: {}", e)
            }
        }
    }

    /// Initialize the callback that is passed to the Windows low level hook to receive key events
    /// and run the native_windows_gui event loop.
    #[cfg(target_os = "windows")]
    pub fn event_loop(kanata: Arc<Mutex<Self>>, tx: Sender<KeyEvent>) -> Result<()> {
        // Display debug and panic output when launched from a terminal.
        unsafe {
            use winapi::um::wincon::*;
            if AttachConsole(ATTACH_PARENT_PROCESS) != 0 {
                panic!("Could not attach to console");
            }
        };
        native_windows_gui::init()?;

        let mapped_keys = {
            let kanata = kanata.lock();
            kanata.mapped_keys
        };

        // This callback should return `false` if the input event is **not** handled by the
        // callback and `true` if the input event **is** handled by the callback. Returning false
        // informs the callback caller that the input event should be handed back to the OS for
        // normal processing.
        let _kbhook = KeyboardHook::set_input_cb(move |input_event| {
            if input_event.code as usize >= cfg::MAPPED_KEYS_LEN {
                return false;
            }
            if !mapped_keys[input_event.code as usize] {
                return false;
            }

            let mut key_event = match KeyEvent::try_from(input_event) {
                Ok(ev) => ev,
                _ => return false,
            };

            // Unlike Linux, Windows does not use a separate value for repeat. However, our code
            // needs to differentiate between initial press and repeat press.
            match key_event.value {
                KeyValue::Release => {
                    PRESSED_KEYS.lock().remove(&key_event.code);
                }
                KeyValue::Press => {
                    if PRESSED_KEYS.lock().contains(&key_event.code) {
                        key_event.value = KeyValue::Repeat;
                    } else {
                        PRESSED_KEYS.lock().insert(key_event.code);
                    }
                }
                _ => {}
            }

            // Send input_events to the processing loop. Panic if channel somehow gets full or if
            // channel disconnects. Typing input should never trigger a panic based on the channel
            // getting full, assuming regular operation of the program and some other bug isn't the
            // problem. I've tried to crash the program by pressing as many keys on my keyboard at
            // the same time as I could, but was unable to.
            if let Err(e) = tx.try_send(key_event) {
                panic!("failed to send on channel: {:?}", e)
            }
            true
        });

        // The event loop is also required for the low-level keyboard hook to work.
        native_windows_gui::dispatch_thread_events();
        Ok(())
    }
}