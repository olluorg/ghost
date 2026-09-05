//! Локальный whisper.cpp. Пакетный режим: весь буфер разом по отпусканию
//! клавиши. Потоковый (LocalAgreement-2) — фаза 2, см. §7.1 SPEC.

use std::path::Path;

use anyhow::{Context, Result};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

use super::{Transcript, Transcriber};

pub struct Whisper {
    state: WhisperState,
    language: String,
    threads: i32,
    /// Список ожидаемых терминов. Пустая строка — затравки нет.
    glossary: String,
    /// Пороги разбора и сколько попыток делать. Читаются на каждом куске:
    /// подбирать их иначе пришлось бы перезапуском.
    tuning: crate::config::Stt,
}

impl Whisper {
    pub fn set_hints(&mut self, glossary: String, tuning: crate::config::Stt) {
        self.glossary = glossary;
        self.tuning = tuning;
    }

    pub fn load(model: &Path, language: &str, threads: usize) -> Result<Self> {
        let ctx = WhisperContext::new_with_params(model, WhisperContextParameters::default())
            .with_context(|| format!("не загрузилась модель {}", model.display()))?;
        // WhisperState держит Arc на контекст, поэтому переживает его дропа.
        let state = ctx.create_state().context("create_state")?;
        Ok(Self {
            state,
            language: language.to_string(),
            threads: threads as i32,
            glossary: String::new(),
            tuning: crate::config::Stt::default(),
        })
    }
}

impl Transcriber for Whisper {
    fn transcribe(&mut self, pcm16k: &[f32]) -> Result<Transcript> {
        // Greedy, а не BeamSearch: beam точнее, но кратно дороже по времени,
        // а здесь важнее задержка. `best_of` при этом больше единицы: он
        // тратится только на кусках, которые whisper признал негодными, и без
        // него откат по температуре не делает ничего.
        let mut params =
            FullParams::new(SamplingStrategy::Greedy { best_of: self.tuning.best_of as i32 });
        params.set_language(Some(&self.language));
        params.set_n_threads(self.threads);
        params.set_translate(false);
        // Пороги отбраковки. Ими же лечится главная беда на тишине: whisper
        // выдаёт заученные фразы из титров, и ловить их списком готовых строк
        // (см. `clean`) можно только задним числом.
        params.set_no_speech_thold(self.tuning.no_speech_thold);
        params.set_entropy_thold(self.tuning.entropy_thold);
        params.set_logprob_thold(self.tuning.logprob_thold);
        // «Музыка», «смех», «аплодисменты» — не речь, а разметка, которой в
        // вопросе к модели быть не должно.
        params.set_suppress_nst(true);
        // Каждая реплика самостоятельна: контекст прошлой только тянет за собой
        // её ошибки.
        params.set_no_context(true);
        params.set_suppress_blank(true);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        // Затравка смещает декодирование к ожидаемым словам. Без неё
        // незнакомый термин записывается фонетически.
        if !self.glossary.is_empty() {
            params.set_initial_prompt(&self.glossary);
        }

        // Громкость входа не наша: канал собеседника приходит таким, каким его
        // отдала система, и на тихой записи whisper ошибается заметно чаще.
        let pcm16k = normalized(pcm16k);
        self.state.full(params, &pcm16k).context("whisper full")?;

        let mut words: Vec<(String, f32)> = Vec::new();
        let mut speaker_turn = false;

        for segment in self.state.as_iter() {
            speaker_turn |= segment.next_segment_speaker_turn();

            for i in 0..segment.n_tokens() {
                let Some(token) = segment.get_token(i) else { continue };
                let Ok(text) = token.to_str_lossy() else { continue };
                // Служебные токены — таймстемпы и маркеры — в текст не идут.
                if text.starts_with("[_") || text.starts_with("<|") {
                    continue;
                }
                let probability = token.token_probability();

                // Токены — куски слов, и уверенность у них разная. Слово судим
                // по худшему куску: одна невнятная морфема портит всё слово.
                if text.starts_with(' ') || words.is_empty() {
                    words.push((text.to_string(), probability));
                } else if let Some(last) = words.last_mut() {
                    last.0.push_str(&text);
                    last.1 = last.1.min(probability);
                }
            }
        }

        Ok(Transcript { text: assemble(&words, self.tuning.confidence), speaker_turn })
    }
}

/// Приводит громкость к рабочему уровню.
///
/// Тихое не усиливаем до бесконечности: на почти пустом куске это подняло бы
/// один шум, а из шума whisper охотно сочиняет фразы. Достаточно громкое
/// оставляем как есть — трогать его незачем.
fn normalized(pcm: &[f32]) -> std::borrow::Cow<'_, [f32]> {
    const TARGET: f32 = 0.9;
    /// Ниже этого пика в куске нет речи, а есть тишина и шум.
    const FLOOR: f32 = 0.02;
    /// Потолок усиления: за ним начинается вытягивание шума.
    const MAX_GAIN: f32 = 8.0;

    let peak = pcm.iter().fold(0.0_f32, |acc, v| acc.max(v.abs()));
    if peak < FLOOR || peak >= TARGET {
        return std::borrow::Cow::Borrowed(pcm);
    }
    let gain = (TARGET / peak).min(MAX_GAIN);
    std::borrow::Cow::Owned(pcm.iter().map(|v| (v * gain).clamp(-1.0, 1.0)).collect())
}

/// Собирает текст из слов, помечая сомнительные.
///
/// Если сомнительно почти всё — распознавание просто не удалось, и пометки
/// только зашумят вопрос. Тогда отдаём как есть.
fn assemble(words: &[(String, f32)], confidence: f32) -> String {
    // Заученные фразы отсекаем по чистому тексту: пометки уверенности меняют
    // строку, и сравнение со списком переставало срабатывать.
    let raw: String = words.iter().map(|(w, _)| w.as_str()).collect();
    if clean(&raw).is_empty() {
        return String::new();
    }

    let doubtful = words.iter().filter(|(_, p)| *p < confidence).count();
    let mark = !words.is_empty() && doubtful * 2 <= words.len();

    let mut out = String::new();
    for (word, probability) in words {
        if mark && *probability < confidence {
            let trimmed = word.trim_start();
            if word.starts_with(' ') {
                out.push(' ');
            }
            out.push('\u{2039}');
            out.push_str(trimmed);
            out.push('\u{203a}');
        } else {
            out.push_str(word);
        }
    }
    clean(&out)
}

/// На тишине и шуме whisper охотно выдаёт заученные фразы из титров. Отсекаем
/// самые частые, иначе модель получит вопрос, которого никто не задавал.
fn clean(raw: &str) -> String {
    const JUNK: [&str; 6] = [
        "[blank_audio]",
        "продолжение следует...",
        "субтитры сделал dimatorzok",
        "редактор субтитров а.синецкая",
        "корректор а.егорова",
        "thank you.",
    ];

    let text = raw.trim();
    let lowered = text.to_lowercase();
    if JUNK.iter().any(|j| lowered == *j) {
        return String::new();
    }
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(pairs: &[(&str, f32)]) -> Vec<(String, f32)> {
        pairs.iter().map(|(w, p)| ((*w).to_string(), *p)).collect()
    }

    #[test]
    fn уверенные_слова_не_помечаются() {
        let text = assemble(&words(&[("Привет", 0.9), (" мир", 0.8)]), 0.55);
        assert_eq!(text, "Привет мир");
    }

    #[test]
    fn сомнительное_слово_помечается_целиком() {
        // Пометка нужна, чтобы модель не приняла искажённый термин за факт.
        let text = assemble(&words(&[("Что", 0.9), (" такое", 0.9), (" зефирус", 0.2)]), 0.55);
        assert_eq!(text, "Что такое \u{2039}зефирус\u{203a}");
    }

    #[test]
    fn сплошная_неуверенность_не_размечается() {
        // Иначе получится строка из одних скобок, и читать её нельзя ни
        // человеку, ни модели.
        let text = assemble(&words(&[("бу", 0.1), (" бу", 0.1), (" бу", 0.9)]), 0.55);
        assert_eq!(text, "бу бу бу");
    }

    #[test]
    fn галлюцинация_на_тишине_отбрасывается() {
        // Фильтр смотрит на чистый текст: пометки уверенности меняют строку,
        // и раньше он переставал срабатывать.
        let text = assemble(&words(&[("Продолжение", 0.2), (" следует...", 0.3)]), 0.55);
        assert!(text.is_empty(), "заученная фраза не должна попасть в вопрос");
    }

    #[test]
    fn пустой_ввод_даёт_пустой_текст() {
        assert!(assemble(&[], 0.55).is_empty());
    }

    #[test]
    fn тихая_запись_подтягивается_до_рабочего_уровня() {
        // Канал собеседника приходит таким, каким его отдала система, и на
        // тихой записи whisper ошибается заметно чаще.
        let quiet: Vec<f32> = vec![0.3, -0.3, 0.15];
        let loud = normalized(&quiet);
        assert!((loud[0] - 0.9).abs() < 1e-5, "пик подтянут к рабочему уровню");
        assert!(loud[1] < 0.0, "форма волны не меняется, только громкость");
        assert!((loud[2] - 0.45).abs() < 1e-5, "все отсчёты растут в одно и то же число раз");
    }

    #[test]
    fn усиление_ограничено_сверху() {
        // Совсем тихий кусок до рабочего уровня не дотягиваем: там, где речь
        // едва слышна, разница между ней и шумом мала, и вытянутый шум
        // whisper превращает в выдуманные фразы.
        let faint: Vec<f32> = vec![0.05];
        assert!((normalized(&faint)[0] - 0.4).abs() < 1e-5, "не больше восьми раз");
    }

    #[test]
    fn шум_и_тишина_не_усиливаются() {
        // Из вытянутого шума whisper охотно сочиняет фразы, поэтому почти
        // пустой кусок оставляем как есть.
        let noise: Vec<f32> = vec![0.001, -0.002, 0.0];
        assert_eq!(&*normalized(&noise), &noise[..]);
    }

    #[test]
    fn громкая_запись_не_трогается() {
        let loud: Vec<f32> = vec![0.95, -0.99];
        assert_eq!(&*normalized(&loud), &loud[..]);
    }
}
