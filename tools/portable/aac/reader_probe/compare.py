#!/usr/bin/env python3
"""Compares the TSVs of the Apple and the portable reader probe: `compare.py APPLE.tsv PORTABLE.tsv [files-to-show] [file-regex] [-r]`.
Rows are equal, `refused` (the portable reader answers with a typed AAC refusal where AudioToolbox decodes: `-r` lists them), `missing`
(the portable reader stopped before the scenario, e.g. a refusal at open) or DIFF (listed, grouped by file)."""
import sys, collections, re
def norm(s): return re.sub(r'other/audio stream is unusable after [a-z ]+','POISONED',s)
def load(p):
    d={}
    for line in open(p):
        f=line.rstrip('\n').split('\t')
        if len(f)<7: f+= ['']*(7-len(f))
        f=[norm(x) for x in f]
        d[(f[0],f[1])]=f[2:7]
    return d
def main(apple, port, show, filt, verbose):
    A=load(apple); P=load(port)
    cats=collections.Counter(); bad=collections.defaultdict(list)
    for k,a in sorted(A.items()):
        if filt and not re.search(filt,k[0]): continue
        p=P.get(k)
        if p is None:
            cats['missing']+=1; bad['missing'].append((k,a,p)); continue
        if a==p: cats['equal']+=1; continue
        if 'other/' in p[4] and re.search(r'AAC|unavailable',p[4]) and p[0] in ('err','openerr','cfgerr'):
            cats['refused']+=1; bad['refused'].append((k,a,p)); continue
        cats['DIFF']+=1; bad['DIFF'].append((k,a,p))
    print(dict(cats))
    ids=collections.OrderedDict()
    for kind in ('DIFF','missing','refused'):
        if kind=='refused' and not verbose: continue
        byid=collections.defaultdict(list)
        for k,a,p in bad[kind]: byid[k[0]].append((k[1],a,p))
        print('==',kind,len(bad[kind]),'rows in',len(byid),'files')
        n=0
        for i,rows in byid.items():
            k,a,p=rows[0]
            print(f' {i}: {len(rows)} rows [{",".join(r[0] for r in rows[:6])}]\n    A: {a}\n    P: {p}')
            n+=1
            if n>=show: break
if __name__=='__main__':
    args=[a for a in sys.argv[1:] if not a.startswith('-')]
    main(args[0],args[1],int(args[2]) if len(args)>2 else 20, args[3] if len(args)>3 else None, '-r' in sys.argv)
