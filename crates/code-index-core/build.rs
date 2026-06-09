fn main() {
    // BUILD_NUMBER = Unix timestamp в секундах на момент компиляции.
    // Всегда больше предыдущего → автоматически детектим обновление бинарника.
    // Итоговая версия: "0.9.1.1748866200" (major.minor.patch.build).
    //
    // Без cargo:rerun-if-changed build.rs запускается при каждом cargo build,
    // что и нужно — каждый билд получает уникальный номер.
    let build_number = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    println!("cargo:rustc-env=BUILD_NUMBER={}", build_number);
}
