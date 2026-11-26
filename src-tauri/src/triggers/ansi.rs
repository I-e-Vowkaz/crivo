//! Limpeza de sequências ANSI das linhas de saída dos processos.

/// Tira CSI (`ESC [ … final`), OSC (`ESC ] … BEL` ou `ESC \`) e outros `ESC x`;
/// com `\r` no meio, fica só o texto depois do último (barra de progresso).
pub fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut it = line.chars().peekable();
    let mut reset = false;
    while let Some(c) = it.next() {
        match c {
            '\x1b' => match it.next() {
                Some('[') => {
                    for n in it.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&n) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(n) = it.next() {
                        if n == '\x07' {
                            break;
                        }
                        if n == '\x1b' {
                            if it.peek() == Some(&'\\') {
                                it.next();
                            }
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => reset = true,
            _ => {
                if reset {
                    out.clear();
                    reset = false;
                }
                out.push(c);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_csi_and_osc() {
        assert_eq!(strip_ansi("\x1b[32mDONE\x1b[0m ok"), "DONE ok");
        assert_eq!(strip_ansi("\x1b]0;title\x07x"), "x");
    }

    #[test]
    fn keeps_text_after_last_cr() {
        assert_eq!(strip_ansi("10%\r50%\r100% done"), "100% done");
        assert_eq!(strip_ansi("abc\r"), "abc");
    }
}
