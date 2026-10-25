"""ECE bar simulation behind docs/triage-spec-rationale.md ("The ECE simulation").

How often does the bar ECE <= 0.10 fail an honest (perfectly calibrated) backend, and how
often does it pass an overconfident one? Compares plain l1 ECE, plain l2, and the debiased
l2 estimator spec section 3.4 specifies. Run: python3 -I docs/triage-ece-sim.py
"""
import random, math
rng=random.Random(1)
def ece(c,y,kind):
    N=len(c); bins={}
    for ci,yi in zip(c,y): bins.setdefault(min(int(ci*10),9),[]).append((ci,yi))
    s=0.0
    for v in bins.values():
        n=len(v); a=sum(t[1] for t in v)/n; cc=sum(t[0] for t in v)/n
        if kind=='l1': s+=n/N*abs(a-cc)
        elif kind=='l2': s+=n/N*(a-cc)**2
        else: s+=n/N*((a-cc)**2-(a*(1-a)/(n-1) if n>=2 else 0))
    return s if kind=='l1' else math.sqrt(max(0,s))
def draw(d):
    return rng.uniform(0.3,0.99) if d=='u' else 0.25+0.75*rng.betavariate(5,2)
print("--- calibrated backend; % that FAIL (false fail); u = uniform(0.3,0.99), b = 0.25+0.75*Beta(5,2)")
for d in ('u','b'):
  for N in (50,100,200):
    f={'l1':0,'l2':0,'deb':0}; R=3000
    for _ in range(R):
        c=[draw(d) for _ in range(N)]; y=[1.0 if rng.random()<ci else 0.0 for ci in c]
        for k in f: f[k]+= ece(c,y,k)>0.10
    print(d,N,{k:round(100*v/R,1) for k,v in f.items()})
print("--- miscalibrated: true p = c - gap (overconfident), beta dist; % that PASS (false pass)")
for gap in (0.10,0.15,0.20):
  for N in (100,200):
    f={'l1':0,'l2':0,'deb':0}; R=2000
    for _ in range(R):
        c=[draw('b') for _ in range(N)]; y=[1.0 if rng.random()<max(0,ci-gap) else 0.0 for ci in c]
        for k in f: f[k]+= ece(c,y,k)<=0.10
    print(gap,N,{k:round(100*v/R,1) for k,v in f.items()})
