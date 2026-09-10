#!/usr/bin/env python3
"""Generate CycloneDX 1.5 JSON from the locked Cargo dependency graph."""
import argparse, datetime, json, pathlib, subprocess, uuid, tomllib
p=argparse.ArgumentParser();p.add_argument('output',type=pathlib.Path);args=p.parse_args()
metadata=json.loads(subprocess.check_output(['cargo','metadata','--format-version','1','--all-features','--locked'],text=True))
active={node['id'] for node in metadata['resolve']['nodes']}
refs={pkg['id']:f"pkg:cargo/{pkg['name']}@{pkg['version']}" for pkg in metadata['packages']}
lock=tomllib.loads(pathlib.Path('Cargo.lock').read_text())
hashes={(p['name'],p['version']):p['checksum'] for p in lock['package'] if 'checksum' in p}
components=[]
for pkg in metadata['packages']:
    if pkg['id'] not in active: continue
    component={'type':'library','bom-ref':refs[pkg['id']],'name':pkg['name'],'version':pkg['version'],'purl':refs[pkg['id']]}
    if (pkg['name'], pkg['version']) in hashes: component['hashes']=[{'alg':'SHA-256','content':hashes[(pkg['name'],pkg['version'])]}]
    if pkg.get('license'): component['licenses']=[{'expression':pkg['license']}]
    if pkg.get('repository'): component['externalReferences']=[{'type':'vcs','url':pkg['repository']}]
    components.append(component)
result={'bomFormat':'CycloneDX','specVersion':'1.5','serialNumber':'urn:uuid:'+str(uuid.uuid4()),'version':1,'metadata':{'timestamp':datetime.datetime.now(datetime.timezone.utc).isoformat()},'components':components,'dependencies':[{'ref':refs[n['id']],'dependsOn':[refs[d['pkg']] for d in n['deps']]} for n in metadata['resolve']['nodes']]}
args.output.parent.mkdir(parents=True,exist_ok=True);args.output.write_text(json.dumps(result,indent=2)+'\n')
