//! Selected-text capture through UI Automation (no clipboard use).
use super::protection::password_control;
use super::*;
pub fn capture_selection() -> Result<String, String> {
    use std::{io::Read, os::windows::process::CommandExt, process::Stdio};
    let target = foreground();
    if password_control() {
        return Err("Password or inaccessible focused control cannot be read".into());
    }
    let script = r#"[Console]::OutputEncoding=[System.Text.UTF8Encoding]::new($false); Add-Type -AssemblyName UIAutomationClient; Add-Type -AssemblyName UIAutomationTypes; $element=[System.Windows.Automation.AutomationElement]::FocusedElement; if($null -eq $element -or $element.Current.IsPassword){exit 2}; try{$pattern=$element.GetCurrentPattern([System.Windows.Automation.TextPattern]::Pattern);$ranges=$pattern.GetSelection();$text=($ranges|ForEach-Object{$_.GetText(-1)}) -join '';[Console]::Write($text)}catch{exit 3}"#;
    let mut child = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(0x08000000)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "Selection provider could not start")?;
    let pipe = child.stdout.take().ok_or("Selection pipe unavailable")?;
    let reader = std::thread::spawn(move || {
        let mut bytes = vec![];
        pipe.take(1_048_577).read_to_end(&mut bytes).map(|_| bytes)
    });
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if start.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err("Selected text provider timed out; no clipboard was changed".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let bytes = reader
        .join()
        .map_err(|_| "Selection reader failed")?
        .map_err(|_| "Selection read failed")?;
    if !status.success() {
        return Err("Focused application does not expose selected text through UI Automation; clipboard unchanged".into());
    }
    if bytes.len() > 1_048_576 {
        return Err("Selection exceeds 1 MiB".into());
    }
    if foreground().id != target.id {
        return Err("Focus changed while reading selection".into());
    }
    let text = String::from_utf8(bytes).map_err(|_| "Selection provider returned invalid UTF-8")?;
    if text.is_empty() {
        return Err("No selected text is available".into());
    }
    Ok(text)
}
