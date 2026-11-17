#!/usr/bin/env python3
"""Root-only two-phase migration/runtime role separation; never prints credentials.

prepare: creates isolated roles, assigns schema ownership, stages migration URL;
         deliberately leaves the currently used runtime URL unchanged (bridge release).
switch:  after a healthy bridge image, stages restricted app URL for the NEXT deploy.
No app restart, data restore, admin/password/passkey/TOTP change is performed here.
"""
import argparse
import base64
import hashlib
import hmac
from datetime import datetime, timezone
import fcntl
import json
import os
from pathlib import Path
import secrets
import shlex
import subprocess
import sys
import tempfile
from urllib.parse import quote, urlsplit

APP=Path(os.environ.get('ENDLESSDRAWING_APP_DIR','/opt/endlessdrawing'))
CONFIG=Path(os.environ.get('ENDLESSDRAWING_CONFIG_DIR','/etc/endlessdrawing'))
APP_ROLE='canvas_app'
MIGRATOR='canvas_migrator'

class RoleError(Exception): pass

def qi(value): return '"'+value.replace('"','""')+'"'
def literal(value): return "'"+value.replace("'","''")+"'"

def scram_verifier(password):
    # Generated ASCII hex credentials only (no user passwords / SASLprep ambiguity).
    # PostgreSQL accepts a precomputed verifier: plaintext never enters SQL/logs.
    salt=secrets.token_bytes(16)
    salted=hashlib.pbkdf2_hmac('sha256',password.encode(),salt,4096)
    stored=hashlib.sha256(hmac.digest(salted,b'Client Key','sha256')).digest()
    server=hmac.digest(salted,b'Server Key','sha256')
    encoded=lambda value:base64.b64encode(value).decode('ascii')
    return 'SCRAM-SHA-256$4096:'+encoded(salt)+'$'+encoded(stored)+':'+encoded(server)

def private(path):
    return path.is_file() and not path.is_symlink() and path.stat().st_uid==0 and not path.stat().st_mode&0o077

def read_env(path):
    if not private(path): raise RoleError('Root-owned private runtime.env required')
    text=path.read_text(); values={}
    for line in text.splitlines():
        if not line.strip() or line.lstrip().startswith('#'): continue
        key,sep,raw=line.partition('='); fields=shlex.split(raw,comments=True)
        if not sep or len(fields)!=1 or key in values: raise RoleError('Invalid runtime configuration')
        values[key]=fields[0]
    return text,values

def write_env(path,text,values):
    lines=[]; found=set()
    for line in text.splitlines():
        key=line.partition('=')[0]
        if key in values: lines.append(key+'='+values[key]); found.add(key)
        else: lines.append(line)
    lines.extend(key+'='+value for key,value in values.items() if key not in found)
    with tempfile.NamedTemporaryFile(mode='w',dir=path.parent,prefix='.runtime-',delete=False) as output:
        temporary=Path(output.name); output.write('\n'.join(lines)+'\n'); output.flush(); os.fsync(output.fileno())
    temporary.chmod(0o600); temporary.replace(path)

def main():
    os.umask(0o077)
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('operation',choices=['prepare','switch','status'])
    args=parser.parse_args()
    if os.geteuid()!=0: raise RoleError('Run as root on the application VPS')
    text,values=read_env(CONFIG/'runtime.env')
    image=(APP/'current-image').read_text().strip()
    env={**os.environ,'ENDLESSDRAWING_IMAGE':image}
    compose=['docker','compose','--env-file',str(CONFIG/'runtime.env'),'-f',str(APP/'compose.yml')]
    with (APP/'deploy.lock').open('a') as lock:
        try: fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
        except BlockingIOError: raise RoleError('Deployment/backup active; retry later') from None
        with tempfile.NamedTemporaryFile(mode='wb',prefix='db-role-',suffix='.log',dir=CONFIG,delete=False) as log:
            log_path=Path(log.name)
            def sql(statement):
                command=compose+['exec','-T','postgres','sh','-c','exec psql -X -q -At -v ON_ERROR_STOP=1 -U "$POSTGRES_USER" -d "$POSTGRES_DB"']
                sensitive='PASSWORD ' in statement
                if sensitive: statement='SET log_min_error_statement=PANIC; SET log_statement=none; SET log_min_duration_statement=-1; SET log_min_duration_sample=-1; SET log_transaction_sample_rate=0;\n'+statement
                result=subprocess.run(command,input=statement.encode(),stdout=subprocess.PIPE,stderr=subprocess.DEVNULL if sensitive else log,env=env,timeout=90)
                if result.returncode: raise RoleError('Database role operation failed; inspect root-only db-role operation log')
                return result.stdout.decode().strip()
            current=urlsplit(values['DATABASE_URL'])
            db=values['POSTGRES_DB']
            if args.operation=='status':
                result=sql("SELECT json_build_object('role',rolname,'superuser',rolsuper,'createdb',rolcreatedb,'createrole',rolcreaterole,'replication',rolreplication,'bypassrls',rolbypassrls) FROM pg_roles WHERE rolname IN ('canvas_app','canvas_migrator');")
                print(result or 'Dedicated roles not prepared')
                print('Runtime DB role:',current.username)
                log.close(); log_path.unlink(); return
            state_path=CONFIG/'db-role-state.json'
            if args.operation=='prepare':
                if current.username==APP_ROLE: raise RoleError('Runtime already switched; do not repeat bridge preparation')
                if current.hostname!='postgres' or current.username!=values['POSTGRES_USER']:
                    raise RoleError('Review nonstandard maintenance/runtime configuration manually')
                if sql("SELECT rolsuper FROM pg_roles WHERE rolname=current_user")!='t':
                    raise RoleError('Closed maintenance role required for one-time ownership transfer')
                if sql('SHOW log_statement')!='none':
                    raise RoleError('Disable statement logging before role credential setup')
                if state_path.exists():
                    if not private(state_path): raise RoleError('Private role state required')
                    state=json.loads(state_path.read_text())
                else:
                    if sql("SELECT count(*) FROM pg_roles WHERE rolname IN ('canvas_app','canvas_migrator')")!='0':
                        raise RoleError('Role names already exist without our private state; manual review required')
                    state={'app_password':secrets.token_hex(32),'migration_password':secrets.token_hex(32)}
                    with state_path.open('x') as output:
                        json.dump(state,output); output.flush(); os.fsync(output.fileno())
                    state_path.chmod(0o600)
                if set(state)!={'app_password','migration_password'} or any(not isinstance(v,str) or len(v)!=64 or any(c not in '0123456789abcdef' for c in v) for v in state.values()):
                    raise RoleError('Invalid private role state')
                for role in [APP_ROLE,MIGRATOR]:
                    if sql('SELECT count(*) FROM pg_auth_members WHERE member=(SELECT oid FROM pg_roles WHERE rolname='+literal(role)+')')!='0':
                        raise RoleError('Unexpected dedicated-role membership; manual review required')
                statements=['BEGIN;']
                for role,key in [(APP_ROLE,'app_password'),(MIGRATOR,'migration_password')]:
                    statements.append('DO $$ BEGIN IF NOT EXISTS(SELECT 1 FROM pg_roles WHERE rolname='+literal(role)+') THEN CREATE ROLE '+qi(role)+'; END IF; END $$;')
                    statements.append('ALTER ROLE '+qi(role)+' LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS PASSWORD '+literal(scram_verifier(state[key]))+';')
                statements.extend([
                    'GRANT CONNECT ON DATABASE '+qi(db)+' TO '+qi(APP_ROLE)+','+qi(MIGRATOR)+';',
                    'GRANT CREATE ON DATABASE '+qi(db)+' TO '+qi(MIGRATOR)+';',
                    'REVOKE CREATE ON SCHEMA public FROM PUBLIC;',
                    'ALTER SCHEMA public OWNER TO '+qi(MIGRATOR)+';',
                ])
                tables=sql("SELECT relname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND relkind IN ('r','p') AND NOT EXISTS(SELECT 1 FROM pg_depend d WHERE d.objid=c.oid AND d.deptype='e') ORDER BY relname").splitlines()
                sequences=sql("SELECT relname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND relkind='S' ORDER BY relname").splitlines()
                if '_sqlx_migrations' not in tables or 'strokes' not in tables: raise RoleError('Apply the existing schema before separating roles')
                for table in tables:
                    statements.append('ALTER TABLE public.'+qi(table)+' OWNER TO '+qi(MIGRATOR)+';')
                    if table!='_sqlx_migrations': statements.append('GRANT SELECT,INSERT,UPDATE,DELETE ON public.'+qi(table)+' TO '+qi(APP_ROLE)+';')
                for sequence in sequences: statements.append('ALTER SEQUENCE public.'+qi(sequence)+' OWNER TO '+qi(MIGRATOR)+';')
                statements.extend(['GRANT USAGE ON SCHEMA public TO '+qi(APP_ROLE)+';',
                    'GRANT USAGE,SELECT ON ALL SEQUENCES IN SCHEMA public TO '+qi(APP_ROLE)+';',
                    'REVOKE ALL ON public._sqlx_migrations FROM '+qi(APP_ROLE)+';',
                    'GRANT SELECT ON public._sqlx_migrations TO '+qi(APP_ROLE)+';', 'COMMIT;'])
                sql('\n'.join(statements))
                migration_url='postgres://'+MIGRATOR+':'+state['migration_password']+'@postgres:5432/'+quote(db,safe='')
                write_env(CONFIG/'runtime.env',text,{'MIGRATION_DATABASE_URL':migration_url,'RUNTIME_DATABASE_ROLE':APP_ROLE,
                    'DATABASE_MIGRATE_ON_START':'false','DATABASE_REQUIRE_RUNTIME_ROLE':'false'})
                print('Dedicated migration/runtime roles and DML grants prepared. Runtime URL deliberately unchanged; deploy bridge image next.')
            else:
                if not private(state_path): raise RoleError('Prepare roles first')
                state=json.loads(state_path.read_text())
                capability=subprocess.check_output(['docker','image','inspect',image,'-f','{{index .Config.Labels "io.endlessdrawing.db-roles-version"}}'],text=True,stderr=log).strip()
                if capability!='1': raise RoleError('Healthy bridge image with separated migration support must be current before switching')
                backend=subprocess.check_output(compose+['ps','-q','backend'],env=env,text=True,stderr=log).strip()
                if not backend or '\n' in backend: raise RoleError('Exactly one healthy bridge backend required')
                running=subprocess.check_output(['docker','inspect',backend,'-f','{{.Config.Image}} {{.State.Health.Status}}'],text=True,stderr=log).strip()
                if running!=image+' healthy': raise RoleError('Current image must be the healthy running bridge backend')
                # Real TCP/SCRAM app login; password only on stdin, never argv/stdout.
                command=compose+['exec','-T','postgres','sh','-c','IFS= read -r PGPASSWORD; export PGPASSWORD; exec psql -X -At -v ON_ERROR_STOP=1 -h 127.0.0.1 -U canvas_app -d "$POSTGRES_DB" -c "SELECT NOT(rolsuper OR rolcreatedb OR rolcreaterole OR rolreplication OR rolbypassrls) AND NOT has_schema_privilege(current_user,\'public\',\'CREATE\') AND NOT has_database_privilege(current_user,current_database(),\'CREATE\') FROM pg_roles WHERE rolname=current_user"']
                result=subprocess.run(command,input=(state['app_password']+'\n').encode(),env=env,stdout=subprocess.PIPE,stderr=log,timeout=30)
                if result.returncode or result.stdout.strip()!=b't': raise RoleError('Restricted app TCP login/policy validation failed; runtime URL unchanged')
                backup=CONFIG/('runtime.env.before-runtime-role-'+datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ'))
                with backup.open('x') as output: output.write(text)
                backup.chmod(0o600)
                runtime_url='postgres://'+APP_ROLE+':'+state['app_password']+'@postgres:5432/'+quote(db,safe='')
                write_env(CONFIG/'runtime.env',text,{'DATABASE_URL':runtime_url,'DATABASE_MIGRATE_ON_START':'false','DATABASE_REQUIRE_RUNTIME_ROLE':'true'})
                print('Restricted runtime URL staged. Apply ONLY through the next manual Actions deploy; bridge image remains a compatible rollback.')
            log.close(); log_path.unlink()

if __name__=='__main__':
    try: main()
    except RoleError as error:
        print('DB role setup: '+str(error),file=sys.stderr); sys.exit(1)
    except (OSError,ValueError,KeyError,TypeError,subprocess.SubprocessError):
        print('DB role setup failed; no credentials/DSNs/query bodies were printed. Inspect root-only operation log.',file=sys.stderr); sys.exit(1)
