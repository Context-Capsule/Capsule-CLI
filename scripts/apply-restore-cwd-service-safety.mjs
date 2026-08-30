import { readFile, writeFile } from 'node:fs/promises';
import { resolve } from 'node:path';

const root = resolve(import.meta.dirname, '..');

async function replaceOnce(path, before, after) {
  const absolute = resolve(root, path);
  let source = (await readFile(absolute, 'utf8')).replace(/\r\n/g, '\n');
  if (source.includes(after)) {
    console.log(`already safety-patched ${path}`);
    return;
  }
  const first = source.indexOf(before);
  if (first < 0) throw new Error(`${path}: expected safety patch anchor was not found`);
  if (source.indexOf(before, first + before.length) >= 0) throw new Error(`${path}: safety patch anchor is ambiguous`);
  source = source.slice(0, first) + after + source.slice(first + before.length);
  await writeFile(absolute, source, 'utf8');
  console.log(`safety-patched ${path}`);
}

await replaceOnce(
  'src/service_restart.rs',
  `    identity_matches.retain(|session| session.foreground_command.is_none());\n    match identity_matches.as_slice() {\n        [session] => Ok((*session, true)),\n        [] => Err(format!(\n            "restored terminal for {:?} / {} was not observable with an idle compatible PowerShell session",\n            service.host, service.shell\n        )),\n        matches => Err(format!(\n            "refusing to replay saved service #{} because {} idle {:?} / {} PowerShell terminals match its identity but none is at saved CWD {:?}",\n            service.service_index,\n            matches.len(),\n            service.host,\n            service.shell,\n            service.working_directory,\n        )),\n    }`,
  `    // If an independently observed exact PowerShell CWD disagrees with the\n    // service record, trust the terminal observation and refuse to move it. This\n    // protects old capsules whose service CWD may have been poisoned by the old\n    // Win32-process-CWD fallback. Directory recovery is reserved for a unique\n    // idle identity whose current PowerShell CWD is unknown/untrusted.\n    if let Some(trusted_mismatch) = identity_matches.iter().copied().find(|session| {\n        session.foreground_command.is_none()\n            && matches!(\n                session.working_directory_source,\n                WorkingDirectorySource::WindowsTerminalState\n            )\n    }) {\n        return Err(format!(\n            "refusing to move trusted restored PowerShell terminal PID {:?} from exact CWD {:?} to saved service CWD {:?}",\n            trusted_mismatch.pid,\n            trusted_mismatch.working_directory,\n            service.working_directory,\n        ));\n    }\n\n    identity_matches.retain(|session| session.foreground_command.is_none());\n    match identity_matches.as_slice() {\n        [session] => Ok((*session, true)),\n        [] => Err(format!(\n            "restored terminal for {:?} / {} was not observable with an idle compatible PowerShell session",\n            service.host, service.shell\n        )),\n        matches => Err(format!(\n            "refusing to replay saved service #{} because {} idle {:?} / {} PowerShell terminals match its identity but none is at saved CWD {:?}",\n            service.service_index,\n            matches.len(),\n            service.host,\n            service.shell,\n            service.working_directory,\n        )),\n    }`
);

await replaceOnce(
  'src/service_restart.rs',
  `        let service = powershell_service(r"D:\\projects\\capsule");\n        let session = powershell_session(900, r"C:\\Users\\monji");\n        let (selected, recovery) =\n            select_external_service_session(&service, std::slice::from_ref(&session)).unwrap();`,
  `        let service = powershell_service(r"D:\\projects\\capsule");\n        let mut session = powershell_session(900, r"C:\\Users\\monji");\n        session.working_directory_source = WorkingDirectorySource::Unknown;\n        let (selected, recovery) =\n            select_external_service_session(&service, std::slice::from_ref(&session)).unwrap();`
);

await replaceOnce(
  'src/service_restart.rs',
  `        let sessions = vec![\n            powershell_session(900, r"C:\\Users\\monji"),\n            powershell_session(901, r"C:\\Users\\monji"),\n        ];\n        let error = select_external_service_session(&service, &sessions).unwrap_err();`,
  `        let mut first = powershell_session(900, r"C:\\Users\\monji");\n        first.working_directory_source = WorkingDirectorySource::Unknown;\n        let mut second = powershell_session(901, r"C:\\Users\\monji");\n        second.working_directory_source = WorkingDirectorySource::Unknown;\n        let sessions = vec![first, second];\n        let error = select_external_service_session(&service, &sessions).unwrap_err();`
);

await replaceOnce(
  'src/service_restart.rs',
  `    #[test]\n    fn powershell_directory_literal_escapes_single_quote_before_replay() {`,
  `    #[test]\n    fn trusted_powershell_cwd_mismatch_is_never_overridden_by_service_recovery() {\n        let service = powershell_service(r"D:\\projects\\capsule");\n        let session = powershell_session(900, r"E:\\trusted-live-location");\n        let error = select_external_service_session(&service, std::slice::from_ref(&session))\n            .unwrap_err();\n        assert!(error.contains("refusing to move trusted restored PowerShell terminal"));\n        assert!(error.contains("trusted-live-location"));\n    }\n\n    #[test]\n    fn powershell_directory_literal_escapes_single_quote_before_replay() {`
);

console.log('restore CWD service safety patch staged');
