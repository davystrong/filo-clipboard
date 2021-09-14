pub mod cli;
pub mod clipboard_extras;
pub mod key_utils;
pub mod winapi_abstractions;
pub mod winapi_functions;

use cli::Opts;
use clipboard_win::{formats, Clipboard, EnumFormats, Getter};
use core::ptr;
use key_utils::is_key_pressed;
use std::collections::VecDeque;
use std::ffi::CString;
use std::mem;
use winapi::um::winuser;

use crate::clipboard_extras::{set_all, ClipboardItem};
use crate::{
    key_utils::trigger_keys,
    winapi_functions::{
        add_clipboard_format_listener, create_window_ex_a, register_class_ex_a, register_hotkey,
        remove_clipboard_format_listener, sleep, unregister_hotkey,
    },
};

const MAX_RETRIES: u8 = 10;
const SIMILARITY_THRESHOLD: u8 = 230;

#[derive(Debug, PartialEq)]
enum ComparisonResult {
    Same,
    Similar,
    Different,
}

fn compare_data(
    cb_data: &[ClipboardItem],
    prev_cb_data: &[ClipboardItem],
    threshold: u8,
) -> ComparisonResult {
    match (cb_data.len(), prev_cb_data.len()) {
        (0, 0) => ComparisonResult::Same,
        (0, _) | (_, 0) => ComparisonResult::Different,
        _ => {
            let count_eq = cb_data
                .iter()
                .filter(
                    |x| match prev_cb_data.iter().find(|y| x.format == y.format) {
                        Some(y) => **x == *y,
                        None => false,
                    },
                )
                .count();

            let max_eq = *[cb_data.len(), prev_cb_data.len()].iter().max().unwrap();

            if count_eq == max_eq {
                ComparisonResult::Same
            } else if count_eq * 255 >= max_eq * threshold as usize {
                ComparisonResult::Similar
            } else {
                ComparisonResult::Different
            }
        }
    }
}

fn get_cb_text(cb_data: &[ClipboardItem]) -> String {
    cb_data
        .iter()
        .find(|item| item.format == winuser::CF_TEXT)
        .map(|res| String::from_utf8(res.content.clone()).unwrap())
        .unwrap_or_default()
}

pub fn run(opts: Opts) {
    // Create and register a class
    let class_name = "filo-clipboard_class";
    let window_name = "filo-clipboard";

    let class_name_c_string = CString::new(class_name).unwrap();
    let lp_wnd_class = winuser::WNDCLASSEXA {
        cbSize: mem::size_of::<winuser::WNDCLASSEXA>() as u32,
        lpfnWndProc: Some(winuser::DefWindowProcA),
        hInstance: ptr::null_mut(),
        lpszClassName: class_name_c_string.as_ptr(),
        style: 0,
        cbClsExtra: 0,
        cbWndExtra: 0,
        hIcon: ptr::null_mut(),
        hCursor: ptr::null_mut(),
        hbrBackground: ptr::null_mut(),
        lpszMenuName: ptr::null_mut(),
        hIconSm: ptr::null_mut(),
    };

    register_class_ex_a(&lp_wnd_class).unwrap();

    // Create the message window
    let h_wnd = create_window_ex_a(
        winuser::WS_EX_LEFT,
        class_name,
        window_name,
        0,
        0,
        0,
        0,
        0,
        unsafe { &mut *winuser::HWND_MESSAGE },
        None,
        None,
        None,
    )
    .unwrap();

    // Register the clipboard listener to the message window
    add_clipboard_format_listener(h_wnd).unwrap();
    // let _clipboard_listener = ClipboardListener::add(h_wnd);

    // Register the hotkey listener to the message window
    register_hotkey(
        h_wnd,
        1,
        (winuser::MOD_CONTROL | winuser::MOD_SHIFT) as u32,
        'V' as u32,
    )
    .unwrap();
    // let _hotkey_listener = HotkeyListener::add(
    //     h_wnd,
    //     1,
    //     (winuser::MOD_CONTROL | winuser::MOD_SHIFT) as u32,
    //     'V' as u32,
    // );

    // Event loop
    let mut cb_history = VecDeque::<Vec<_>>::new();
    let mut last_internal_update: Option<Vec<ClipboardItem>> = None;
    let mut skip_clipboard = false;

    let mut lp_msg = winuser::MSG::default();
    #[cfg(debug_assertions)]
    println!("Ready");
    while unsafe { winuser::GetMessageA(&mut lp_msg, h_wnd, 0, 0) != 0 } {
        match lp_msg.message {
            winuser::WM_CLIPBOARDUPDATE => {
                if let Ok(_clip) = Clipboard::new_attempts(10) {
                    let cb_data: Vec<_> = EnumFormats::new()
                        .filter_map(|format| {
                            let mut clipboard_data = Vec::new();
                            if let Ok(bytes) =
                                formats::RawData(format).read_clipboard(&mut clipboard_data)
                            {
                                if bytes != 0 {
                                    return Some(ClipboardItem {
                                        format,
                                        content: clipboard_data,
                                    });
                                }
                            }
                            None
                        })
                        .collect();

                    if !cb_data.is_empty() {
                        if skip_clipboard {
                            skip_clipboard = false;
                        } else {
                            //If let chains would do this far more neatly
                            let prev_item_similarity = last_internal_update
                                .as_ref()
                                .map(|last_update| {
                                    compare_data(&cb_data, last_update, SIMILARITY_THRESHOLD)
                                })
                                .unwrap_or(ComparisonResult::Different);
                            let current_item_similarity = cb_history
                                .front()
                                .map(|last_update| {
                                    compare_data(&cb_data, last_update, SIMILARITY_THRESHOLD)
                                })
                                .unwrap_or(ComparisonResult::Different);

                            match (prev_item_similarity, current_item_similarity) {
                                (_, ComparisonResult::Same) | (ComparisonResult::Same, _) => {}
                                (_, ComparisonResult::Similar) | (ComparisonResult::Similar, _) => {
                                    *cb_history.front_mut().unwrap() = cb_data;
                                    last_internal_update = None;
                                }
                                (ComparisonResult::Different, ComparisonResult::Different) => {
                                    cb_history.push_front(cb_data);
                                    cb_history.truncate(opts.max_history);
                                    last_internal_update = None;
                                }
                            }
                        }
                    }
                }
            }
            winuser::WM_HOTKEY => {
                if lp_msg.wParam == 1 {
                    /*Ctrl + Shift + V*/
                    fn old_state(v_key: i32) -> u32 {
                        match is_key_pressed(v_key) {
                            Ok(false) => winuser::KEYEVENTF_KEYUP,
                            _ => 0,
                        }
                    }

                    let old_control = old_state(winuser::VK_CONTROL);
                    let old_v = old_state('V' as i32);

                    match trigger_keys(
                        &[
                            winuser::VK_SHIFT as u16,
                            winuser::VK_CONTROL as u16,
                            'V' as u16,
                            winuser::VK_CONTROL as u16,
                            'V' as u16,
                            winuser::VK_SHIFT as u16,
                        ],
                        &[
                            winuser::KEYEVENTF_KEYUP,
                            if old_control == 0 {
                                winuser::KEYEVENTF_KEYUP
                            } else {
                                0
                            },
                            if old_v == 0 {
                                winuser::KEYEVENTF_KEYUP
                            } else {
                                0
                            },
                            old_control,
                            old_v,
                            old_state(winuser::VK_SHIFT),
                        ],
                    ) {
                        Ok(_) => {
                            // Sleep for less time than the lowest possible automatic keystroke repeat ((1000ms / 30) * 0.8)
                            sleep(25);
                            last_internal_update = cb_history.pop_front();
                            if let Some(prev_item) = cb_history.front() {
                                skip_clipboard = true;
                                if let Ok(_clip) = Clipboard::new_attempts(10) {
                                    let _ = set_all(prev_item);
                                }
                            }
                        }
                        Err(_) => {
                            let mut retries = 0u8;
                            while let Err(error) = trigger_keys(
                                &[
                                    winuser::VK_SHIFT as u16,
                                    winuser::VK_CONTROL as u16,
                                    'V' as u16,
                                ],
                                &[
                                    winuser::KEYEVENTF_KEYUP,
                                    winuser::KEYEVENTF_KEYUP,
                                    winuser::KEYEVENTF_KEYUP,
                                ],
                            ) {
                                if retries >= MAX_RETRIES {
                                    panic!("Could not release keys after {} attemps. Something has gone badly wrong: {}", MAX_RETRIES, error)
                                }
                                retries += 1;
                                sleep(25);
                            }
                        }
                    }
                }
            }
            _ => unsafe {
                winuser::DefWindowProcA(lp_msg.hwnd, lp_msg.message, lp_msg.wParam, lp_msg.lParam);
            },
        }
    }

    let _ = unregister_hotkey(h_wnd, 1);
    let _ = remove_clipboard_format_listener(h_wnd);
}
