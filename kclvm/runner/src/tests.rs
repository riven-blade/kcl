use crate::assembler::clean_path;
use crate::assembler::KclvmAssembler;
use crate::assembler::KclvmLibAssembler;
use crate::assembler::LibAssembler;
use crate::exec_program;
use crate::temp_file;
use crate::{execute, runner::ExecProgramArgs};
use anyhow::Context;
use anyhow::Result;
use kclvm_ast::ast::{Module, Program};
use kclvm_compiler::codegen::llvm::OBJECT_FILE_SUFFIX;
use kclvm_config::settings::load_file;
use kclvm_parser::load_program;
use kclvm_parser::ParseSession;
use kclvm_sema::resolver::resolve_program;
use std::fs::create_dir_all;
use std::panic::catch_unwind;
use std::panic::set_hook;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::{
    collections::HashMap,
    fs::{self, File},
};
use tempfile::tempdir;
use walkdir::WalkDir;

const MULTI_FILE_TEST_CASES: &[&str; 5] = &[
    "no_kcl_mod_file",
    "relative_import",
    "relative_import_as",
    "import_regular_module",
    "import_regular_module_as",
];

const TEST_CASES: &[&str; 5] = &[
    "init_check_order_0",
    "init_check_order_1",
    "normal_2",
    "type_annotation_not_full_2",
    "multi_vars_0",
];

fn exec_data_path() -> String {
    Path::new("src").join("exec_data").display().to_string()
}

fn exec_err_data_path() -> String {
    Path::new("src").join("exec_err_data").display().to_string()
}

fn custom_manifests_data_path() -> String {
    Path::new("src")
        .join("custom_manifests_data")
        .display()
        .to_string()
}

fn multi_file_test_cases() -> Vec<String> {
    let mut test_cases: Vec<String> = MULTI_FILE_TEST_CASES
        .iter()
        .map(|case| {
            Path::new("multi_file_compilation")
                .join(case)
                .display()
                .to_string()
        })
        .collect();

    test_cases.push(
        Path::new("multi_file_compilation")
            .join("import_abs_path")
            .join("app-main")
            .display()
            .to_string(),
    );
    test_cases.push(
        Path::new("..")
            .join("..")
            .join("..")
            .join("..")
            .join("test")
            .join("integration")
            .join("konfig")
            .join("base")
            .join("examples")
            .join("job-example")
            .join("dev")
            .display()
            .to_string(),
    );

    test_cases
}

fn exec_prog_args_test_case() -> Vec<String> {
    vec![Path::new("exec_prog_args")
        .join("default.json")
        .display()
        .to_string()]
}

fn settings_file_test_case() -> Vec<(String, String)> {
    vec![(
        Path::new("settings_file")
            .join("settings.yaml")
            .display()
            .to_string(),
        Path::new("settings_file")
            .join("settings.json")
            .display()
            .to_string(),
    )]
}

const EXPECTED_JSON_FILE_NAME: &str = "stdout.golden.json";

fn test_case_path() -> String {
    Path::new("src").join("test_datas").display().to_string()
}

const KCL_FILE_NAME: &str = "main.k";
const MAIN_PKG_NAME: &str = "__main__";
const CARGO_PATH: &str = env!("CARGO_MANIFEST_DIR");

#[derive(serde::Deserialize, serde::Serialize)]
struct SimplePanicInfo {
    line: i32,
    col: i32,
    message: String,
}

fn gen_full_path(rel_path: String) -> Result<String> {
    let mut cargo_file_path = PathBuf::from(CARGO_PATH);
    cargo_file_path.push(&rel_path);
    let full_path = cargo_file_path
        .to_str()
        .with_context(|| format!("No such file or directory '{}'", rel_path))?;
    Ok(full_path.to_string())
}

/// Load test kcl file to ast.Program
fn load_test_program(filename: String) -> Program {
    let module = kclvm_parser::parse_file(&filename, None).unwrap();
    construct_program(module)
}

fn parse_program(test_kcl_case_path: &str) -> Program {
    let args = ExecProgramArgs::default();
    let opts = args.get_load_program_options();
    load_program(
        Arc::new(ParseSession::default()),
        &[test_kcl_case_path],
        Some(opts),
    )
    .unwrap()
}

/// Construct ast.Program by ast.Module and default configuration.
/// Default configuration:
///     module.pkg = "__main__"
///     Program.root = "__main__"
///     Program.main = "__main__"
fn construct_program(mut module: Module) -> Program {
    module.pkg = MAIN_PKG_NAME.to_string();
    let mut pkgs_ast = HashMap::new();
    pkgs_ast.insert(MAIN_PKG_NAME.to_string(), vec![module]);
    Program {
        root: MAIN_PKG_NAME.to_string(),
        main: MAIN_PKG_NAME.to_string(),
        pkgs: pkgs_ast,
    }
}

fn construct_pkg_lib_path(
    prog: &Program,
    assembler: &KclvmAssembler,
    main_path: &str,
    suffix: String,
) -> Vec<PathBuf> {
    let cache_dir = assembler.construct_cache_dir(&prog.root);
    let mut result = vec![];
    for (pkgpath, _) in &prog.pkgs {
        if pkgpath == "__main__" {
            result.push(PathBuf::from(format!("{}{}", main_path, suffix)));
        } else {
            result.push(cache_dir.join(format!("{}{}", pkgpath.clone(), suffix)));
        }
    }
    result
}

/// Load the expect result from stdout.golden.json
fn load_expect_file(filename: String) -> String {
    let f = File::open(filename).unwrap();
    let v: serde_json::Value = serde_json::from_reader(f).unwrap();
    v.to_string()
}

/// Format str by json str
fn format_str_by_json(str: String) -> String {
    let v: serde_json::Value = serde_json::from_str(&str).unwrap();
    v.to_string()
}

fn execute_for_test(kcl_path: &String) -> String {
    let args = ExecProgramArgs::default();
    // Parse kcl file
    let program = load_test_program(kcl_path.to_string());
    // Generate libs, link libs and execute.
    execute(Arc::new(ParseSession::default()), program, &args).unwrap()
}

fn gen_assembler(entry_file: &str, test_kcl_case_path: &str) -> KclvmAssembler {
    let mut prog = parse_program(test_kcl_case_path);
    let scope = resolve_program(&mut prog);
    KclvmAssembler::new(
        prog.clone(),
        scope,
        entry_file.to_string(),
        KclvmLibAssembler::LLVM,
    )
}

fn gen_libs_for_test(entry_file: &str, test_kcl_case_path: &str) {
    let assembler = gen_assembler(entry_file, test_kcl_case_path);

    let expected_pkg_paths = construct_pkg_lib_path(
        &parse_program(test_kcl_case_path),
        &assembler,
        PathBuf::from(entry_file).to_str().unwrap(),
        OBJECT_FILE_SUFFIX.to_string(),
    );

    let lib_paths = assembler.gen_libs();

    assert_eq!(lib_paths.len(), expected_pkg_paths.len());

    for pkg_path in &expected_pkg_paths {
        assert_eq!(pkg_path.exists(), true);
    }

    let tmp_main_lib_path =
        fs::canonicalize(format!("{}{}", entry_file, OBJECT_FILE_SUFFIX)).unwrap();
    assert_eq!(tmp_main_lib_path.exists(), true);

    clean_path(tmp_main_lib_path.to_str().unwrap());
    assert_eq!(tmp_main_lib_path.exists(), false);
}

fn assemble_lib_for_test(
    entry_file: &str,
    test_kcl_case_path: &str,
    assembler: &KclvmLibAssembler,
) -> String {
    // default args and configuration
    let mut args = ExecProgramArgs::default();

    args.k_filename_list.push(test_kcl_case_path.to_string());
    let files = args.get_files();
    let opts = args.get_load_program_options();
    let sess = Arc::new(ParseSession::default());
    // parse and resolve kcl
    let mut program = load_program(sess, &files, Some(opts)).unwrap();

    let scope = resolve_program(&mut program);

    // tmp file
    let temp_entry_file_path = &format!("{}{}", entry_file, OBJECT_FILE_SUFFIX);

    // Assemble object files
    assembler.assemble(
        &program,
        scope.import_names,
        entry_file,
        temp_entry_file_path,
    )
}

fn test_kclvm_runner_execute() {
    for case in TEST_CASES {
        let kcl_path = &Path::new(&test_case_path())
            .join(case)
            .join(KCL_FILE_NAME)
            .display()
            .to_string();
        let expected_path = &Path::new(&test_case_path())
            .join(case)
            .join(EXPECTED_JSON_FILE_NAME)
            .display()
            .to_string();
        let result = execute_for_test(kcl_path);
        let expected_result = load_expect_file(expected_path.to_string());
        assert_eq!(expected_result, format_str_by_json(result));
    }
}

fn test_kclvm_runner_execute_timeout() {
    set_hook(Box::new(|_| {}));
    let result_time_out = catch_unwind(|| {
        gen_libs_for_test(
            &Path::new("test")
                .join("no_exist_path")
                .display()
                .to_string(),
            &Path::new(".")
                .join("src")
                .join("test_datas")
                .join("multi_file_compilation")
                .join("import_abs_path")
                .join("app-main")
                .join("main.k")
                .display()
                .to_string(),
        );
    });
    let timeout_panic_msg = "called `Result::unwrap()` on an `Err` value: Timeout";
    match result_time_out {
        Err(panic_err) => {
            if let Some(s) = panic_err.downcast_ref::<String>() {
                assert_eq!(s, timeout_panic_msg)
            }
        }
        _ => {
            unreachable!()
        }
    }
}

#[test]
fn test_assemble_lib_llvm() {
    for case in TEST_CASES {
        let temp_dir = tempdir().unwrap();
        let temp_dir_path = temp_dir.path().to_str().unwrap();
        let temp_entry_file = temp_file(temp_dir_path);
        let kcl_path = &Path::new(&test_case_path())
            .join(case)
            .join(KCL_FILE_NAME)
            .display()
            .to_string();
        let assembler = &KclvmLibAssembler::LLVM;

        let lib_file = assemble_lib_for_test(
            &format!("{}{}", temp_entry_file, "4assemble_lib"),
            kcl_path,
            assembler,
        );

        let lib_path = std::path::Path::new(&lib_file);
        assert_eq!(lib_path.exists(), true);
        clean_path(&lib_file);
        assert_eq!(lib_path.exists(), false);
    }
}

#[test]
fn test_gen_libs() {
    for case in multi_file_test_cases() {
        let temp_dir = tempdir().unwrap();
        let temp_dir_path = temp_dir.path().to_str().unwrap();
        let temp_entry_file = temp_file(temp_dir_path);

        let kcl_path = gen_full_path(
            Path::new(&test_case_path())
                .join(case)
                .join(KCL_FILE_NAME)
                .display()
                .to_string(),
        )
        .unwrap();
        gen_libs_for_test(&format!("{}{}", temp_entry_file, "4gen_libs"), &kcl_path);
    }
}

// Fixme: parallel string/identifier clone panic.
// #[test]
fn _test_gen_libs_parallel() {
    let gen_lib_1 = thread::spawn(|| {
        for _ in 0..9 {
            test_gen_libs();
        }
    });

    let gen_lib_2 = thread::spawn(|| {
        for _ in 0..9 {
            test_gen_libs();
        }
    });

    let gen_lib_3 = thread::spawn(|| {
        for _ in 0..9 {
            test_gen_libs();
        }
    });

    let gen_lib_4 = thread::spawn(|| {
        for _ in 0..9 {
            test_gen_libs();
        }
    });

    gen_lib_1.join().unwrap();
    gen_lib_2.join().unwrap();
    gen_lib_3.join().unwrap();
    gen_lib_4.join().unwrap();
}

#[test]
fn test_clean_path_for_genlibs() {
    let mut prog = parse_program(
        &Path::new(".")
            .join("src")
            .join("test_datas")
            .join("multi_file_compilation")
            .join("import_abs_path")
            .join("app-main")
            .join("main.k")
            .display()
            .to_string(),
    );
    let scope = resolve_program(&mut prog);
    let assembler = KclvmAssembler::new(prog, scope, String::new(), KclvmLibAssembler::LLVM);

    let temp_dir = tempdir().unwrap();
    let temp_dir_path = temp_dir.path().to_str().unwrap();
    let tmp_file_path = &temp_file(temp_dir_path);

    create_dir_all(tmp_file_path).unwrap();

    let file_name = &Path::new(tmp_file_path).join("test").display().to_string();
    let file_suffix = ".o";

    File::create(file_name).unwrap();
    let path = std::path::Path::new(file_name);
    assert_eq!(path.exists(), true);

    assembler.clean_path_for_genlibs(file_name, file_suffix);
    assert_eq!(path.exists(), false);

    let test1 = &format!("{}{}", file_name, ".test1.o");
    let test2 = &format!("{}{}", file_name, ".test2.o");
    File::create(test1).unwrap();
    File::create(test2).unwrap();
    let path1 = std::path::Path::new(test1);

    let path2 = std::path::Path::new(test2);
    assert_eq!(path1.exists(), true);
    assert_eq!(path2.exists(), true);

    assembler.clean_path_for_genlibs(file_name, file_suffix);
    assert_eq!(path1.exists(), false);
    assert_eq!(path2.exists(), false);
}

#[test]
fn test_to_json_program_arg() {
    for case in exec_prog_args_test_case() {
        let test_case_json_file = &Path::new(&test_case_path())
            .join(case)
            .display()
            .to_string();
        let expected_json_str = fs::read_to_string(test_case_json_file).unwrap();
        let exec_prog_args = ExecProgramArgs::default();
        assert_eq!(expected_json_str.trim(), exec_prog_args.to_json().trim());
    }
}

#[test]
fn test_from_str_program_arg() {
    for case in exec_prog_args_test_case() {
        let test_case_json_file = &Path::new(&test_case_path())
            .join(case)
            .display()
            .to_string();
        let expected_json_str = fs::read_to_string(test_case_json_file).unwrap();
        let exec_prog_args = ExecProgramArgs::from_str(&expected_json_str);
        assert_eq!(expected_json_str.trim(), exec_prog_args.to_json().trim());
    }
}

#[test]
fn test_from_setting_file_program_arg() {
    for (case_yaml, case_json) in settings_file_test_case() {
        let test_case_yaml_file = &Path::new(&test_case_path())
            .join(case_yaml)
            .display()
            .to_string();
        let settings_file = load_file(test_case_yaml_file).unwrap();

        let test_case_json_file = &Path::new(&test_case_path())
            .join(case_json)
            .display()
            .to_string();
        let expected_json_str = fs::read_to_string(test_case_json_file).unwrap();

        let exec_prog_args = ExecProgramArgs::try_from(settings_file).unwrap();
        assert_eq!(expected_json_str.trim(), exec_prog_args.to_json().trim());
    }
}

fn test_exec_file() {
    let prev_hook = std::panic::take_hook();
    // disable print panic info
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(|| {
        for file in get_files(exec_data_path(), false, true, ".k") {
            exec(&file).unwrap();
            println!("{} - PASS", file);
        }
    });
    assert!(result.is_ok());
    std::panic::set_hook(prev_hook);
}

fn test_custom_manifests_output() {
    exec_with_result_at(&custom_manifests_data_path());
}

fn test_exec_with_err_result() {
    exec_with_err_result_at(&exec_err_data_path());
}

fn clean_dir(path: String) {
    match fs::remove_dir_all(path) {
        Ok(_) => {}
        Err(_) => {}
    }
}

#[test]
fn test_exec() {
    clean_dir(
        Path::new(".")
            .join("src")
            .join("exec_data")
            .join(".kclvm")
            .display()
            .to_string(),
    );

    clean_dir(
        Path::new(".")
            .join("src")
            .join("exec_err_data")
            .join(".kclvm")
            .display()
            .to_string(),
    );

    test_exec_file();
    println!("test_exec_file - PASS");

    test_kclvm_runner_execute();
    println!("test_kclvm_runner_execute - PASS");

    test_kclvm_runner_execute_timeout();
    println!("test_kclvm_runner_execute_timeout - PASS");
    fs::remove_dir_all(Path::new("__main__")).unwrap();

    test_custom_manifests_output();
    println!("test_custom_manifests_output - PASS");

    test_exec_with_err_result();
    println!("test_exec_with_err_result - PASS");
}

fn exec(file: &str) -> Result<String, String> {
    let mut args = ExecProgramArgs::default();
    args.k_filename_list.push(file.to_string());
    let opts = args.get_load_program_options();
    let sess = Arc::new(ParseSession::default());
    // Load AST program
    let program = load_program(sess.clone(), &[file], Some(opts)).unwrap();
    // Resolve ATS, generate libs, link libs and execute.
    execute(sess, program, &args)
}

/// Run all kcl files at path and compare the exec result with the expect output.
fn exec_with_result_at(path: &str) {
    let kcl_files = get_files(path, false, true, ".k");
    let output_files = get_files(path, false, true, ".stdout.golden");
    for (kcl_file, output_file) in kcl_files.iter().zip(&output_files) {
        let mut args = ExecProgramArgs::default();
        args.k_filename_list.push(kcl_file.to_string());
        let result = exec_program(Arc::new(ParseSession::default()), &args).unwrap();

        #[cfg(not(target_os = "windows"))]
        let newline = "\n";
        #[cfg(target_os = "windows")]
        let newline = "\r\n";

        let expected = std::fs::read_to_string(output_file)
            .unwrap()
            .strip_suffix(newline)
            .unwrap()
            .to_string();

        #[cfg(target_os = "windows")]
        let expected = expected.replace("\r\n", "\n");

        assert_eq!(result.yaml_result, expected);
    }
}

/// Run all kcl files at path and compare the exec error result with the expect error output.
fn exec_with_err_result_at(path: &str) {
    let kcl_files = get_files(path, false, true, ".k");
    let output_files = get_files(path, false, true, ".stderr.json");

    let prev_hook = std::panic::take_hook();
    // disable print panic info
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(|| {
        for (kcl_file, _) in kcl_files.iter().zip(&output_files) {
            let mut args = ExecProgramArgs::default();
            args.k_filename_list.push(kcl_file.to_string());
            assert!(exec_program(Arc::new(ParseSession::default()), &args).is_err());
        }
    });
    assert!(result.is_ok());
    std::panic::set_hook(prev_hook);
}

/// Get kcl files from path.
fn get_files<P: AsRef<Path>>(
    path: P,
    recursively: bool,
    sorted: bool,
    suffix: &str,
) -> Vec<String> {
    let mut files = vec![];
    for entry in WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_file() {
            let file = path.to_str().unwrap();
            if file.ends_with(suffix) && (recursively || entry.depth() == 1) {
                files.push(file.to_string())
            }
        }
    }
    if sorted {
        files.sort();
    }
    files
}
