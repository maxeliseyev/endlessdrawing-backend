"""Role setup privacy/state machine tests. No live DB, passwords or VPS touched."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import stat
import tempfile
import types
import unittest
from unittest import mock

SPEC=importlib.util.spec_from_file_location('db_roles',Path(__file__).resolve().parents[2]/'deploy/db-roles.py')
roles=importlib.util.module_from_spec(SPEC); SPEC.loader.exec_module(roles)

class RoleSetupTests(unittest.TestCase):
    def setUp(self):
        temporary=tempfile.TemporaryDirectory(); self.addCleanup(temporary.cleanup)
        self.root=Path(temporary.name); self.app=self.root/'app'; self.config=self.root/'config'
        self.app.mkdir(); self.config.mkdir()
        self.image='endlessdrawing-app:'+'a'*40+'-1-1'
        (self.app/'current-image').write_text(self.image+'\n')
        self.runtime=self.config/'runtime.env'
        self.original='POSTGRES_DB=canvas\nPOSTGRES_USER=canvas\nPOSTGRES_PASSWORD=fixture-secret\nDATABASE_URL=postgres://canvas:fixture-secret@postgres:5432/canvas\n'
        self.runtime.write_text(self.original); self.runtime.chmod(0o600)
        self.calls=[]; self.existing_roles=False; self.login_ok=True; self.capability='1'; self.healthy=True; self.fail_sql=False
        self.stack=contextlib.ExitStack(); self.addCleanup(self.stack.close)
        self.stack.enter_context(mock.patch.object(roles,'APP',self.app))
        self.stack.enter_context(mock.patch.object(roles,'CONFIG',self.config))
        self.stack.enter_context(mock.patch.object(roles.os,'geteuid',return_value=0))
        # CI need not run as root. Ownership behavior itself is tested independently.
        self.stack.enter_context(mock.patch.object(roles,'private',side_effect=lambda p:p.is_file() and not p.is_symlink() and not p.stat().st_mode&0o077))
        self.stack.enter_context(mock.patch.object(roles.subprocess,'run',side_effect=self.run_process))
        self.stack.enter_context(mock.patch.object(roles.subprocess,'check_output',side_effect=self.output))

    def run_process(self,args,**kwargs):
        self.calls.append((args,kwargs.get('input',b'')))
        self.assertNotIn('fixture-secret',' '.join(args))
        if 'IFS= read -r PGPASSWORD' in ' '.join(args):
            return types.SimpleNamespace(returncode=0 if self.login_ok else 1,stdout=b't\n' if self.login_ok else b'')
        query=kwargs['input'].decode()
        result=''
        if 'SELECT current_database()' in query: result='canvas'
        elif 'SELECT rolsuper' in query: result='t'
        elif 'SHOW log_statement' in query: result='none'
        elif 'SELECT count(*) FROM pg_roles' in query: result='1' if self.existing_roles else '0'
        elif 'SELECT count(*) FROM pg_auth_members' in query: result='0'
        elif "relkind IN ('r','p')" in query: result='_sqlx_migrations\nadmin_accounts\nstrokes'
        elif "relkind='S'" in query: result='strokes_seq_seq'
        return types.SimpleNamespace(returncode=1 if self.fail_sql else 0,stdout=result.encode())

    def output(self,args,**kwargs):
        if args[:3]==['docker','image','inspect']: return self.capability+'\n'
        if args[:2]==['docker','inspect']: return self.image+(' healthy\n' if self.healthy else ' unhealthy\n')
        return 'backend-fixture\n'

    def invoke(self,operation):
        output=io.StringIO()
        with mock.patch('sys.argv',['db-roles',operation]),contextlib.redirect_stdout(output): roles.main()
        text=output.getvalue()
        self.assertNotIn('fixture-secret',text)
        state=self.config/'db-role-state.json'
        if state.exists():
            for password in json.loads(state.read_text()).values():
                self.assertNotIn(password,text)
                for args,_ in self.calls: self.assertNotIn(password,' '.join(args))
        return text

    def test_prepare_changes_only_migration_url_and_never_restarts_or_outputs_secrets(self):
        self.invoke('prepare')
        _,values=roles.read_env(self.runtime)
        self.assertEqual(values['DATABASE_URL'],'postgres://canvas:fixture-secret@postgres:5432/canvas')
        self.assertTrue(values['MIGRATION_DATABASE_URL'].startswith('postgres://canvas_migrator:'))
        self.assertEqual(values['DATABASE_REQUIRE_RUNTIME_ROLE'],'false')
        self.assertEqual(stat.S_IMODE((self.config/'db-role-state.json').stat().st_mode),0o600)
        statements='\n'.join(body.decode() for _,body in self.calls)
        self.assertIn('NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS',statements)
        self.assertIn('SCRAM-SHA-256$4096:',statements)
        for password in json.loads((self.config/'db-role-state.json').read_text()).values(): self.assertNotIn(password,statements)
        self.assertIn('ALTER TABLE public."strokes" OWNER TO "canvas_migrator"',statements)
        self.assertIn('REVOKE ALL ON public._sqlx_migrations FROM "canvas_app"',statements)
        self.assertFalse(any('up' in args for args,_ in self.calls))

    def test_switch_requires_capable_healthy_bridge_and_valid_tcp_login(self):
        self.invoke('prepare')
        before=self.runtime.read_text()
        for attribute,value in [('capability',''),('healthy',False),('login_ok',False)]:
            original=getattr(self,attribute); setattr(self,attribute,value)
            with self.assertRaises(roles.RoleError): self.invoke('switch')
            self.assertEqual(self.runtime.read_text(),before)
            setattr(self,attribute,original)
        self.invoke('switch')
        _,values=roles.read_env(self.runtime)
        self.assertTrue(values['DATABASE_URL'].startswith('postgres://canvas_app:'))
        self.assertEqual(values['DATABASE_MIGRATE_ON_START'],'false')
        self.assertEqual(values['DATABASE_REQUIRE_RUNTIME_ROLE'],'true')
        backups=list(self.config.glob('runtime.env.before-runtime-role-*'))
        self.assertEqual(len(backups),1); self.assertEqual(backups[0].read_text(),before)
        self.assertEqual(stat.S_IMODE(backups[0].stat().st_mode),0o600)
        with self.assertRaises(roles.RoleError): self.invoke('prepare')

    def test_mismatched_runtime_database_refused_before_role_changes(self):
        self.runtime.write_text(self.original.replace('@postgres:5432/canvas','@postgres:5432/other'))
        with self.assertRaises(roles.RoleError): self.invoke('prepare')
        self.assertFalse((self.config/'db-role-state.json').exists())
        self.assertFalse(any(b'ALTER ROLE' in body for _,body in self.calls))

    def test_unknown_existing_roles_are_not_overwritten(self):
        self.existing_roles=True
        with self.assertRaises(roles.RoleError): self.invoke('prepare')
        self.assertEqual(self.runtime.read_text(),self.original)
        self.assertFalse((self.config/'db-role-state.json').exists())
        self.assertFalse(any(b'ALTER ROLE' in body for _,body in self.calls))

    def test_failed_database_step_leaves_runtime_and_private_diagnostics(self):
        self.fail_sql=True
        with self.assertRaises(roles.RoleError): self.invoke('prepare')
        self.assertEqual(self.runtime.read_text(),self.original)
        for file in self.config.glob('db-role-*.log'): self.assertEqual(stat.S_IMODE(file.stat().st_mode),0o600)

    def test_nonroot_refused_before_role_or_credential_setup(self):
        with mock.patch.object(roles.os,'geteuid',return_value=1001),self.assertRaises(roles.RoleError): self.invoke('prepare')
        self.assertEqual(self.calls,[])

    def test_env_parser_is_not_shell_and_duplicate_keys_rejected(self):
        self.runtime.write_text(self.original+'POSTGRES_DB=other\n')
        with self.assertRaises(roles.RoleError): roles.read_env(self.runtime)
        self.assertEqual(roles.qi('unsafe"name'),'"unsafe""name"')
        self.assertEqual(roles.literal("a'b"),"'a''b'")


class PrivateRoleFileTests(unittest.TestCase):
    def test_private_owner_permissions_and_symlink_checks(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'config'; path.write_text('fixture'); path.chmod(0o600)
            for uid,mode,expected in [(0,0o600,True),(1001,0o600,False),(0,0o644,False)]:
                fake=types.SimpleNamespace(st_uid=uid,st_mode=stat.S_IFREG|mode)
                with mock.patch.object(Path,'stat',return_value=fake): self.assertEqual(roles.private(path),expected)
            link=Path(directory)/'link'; link.symlink_to(path)
            self.assertFalse(roles.private(link))
