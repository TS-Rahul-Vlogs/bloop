import { useState } from 'react';
import Code from '../CodeBlock/Code';
import Button from '../Button';
import { CheckIcon, Clipboard } from '../../icons';
import { copyToClipboard } from '../../utils';

type Props = {
  code: string;
  language: string;
  isSummary?: boolean;
};

const NewCode = ({ code, language, isSummary }: Props) => {
  const [codeCopied, setCodeCopied] = useState(false);
  return (
    <div
      className={`${
        !isSummary ? 'my-4 p-4 bg-bg-shade' : 'bg-chat-bg-sub'
      } text-sm pr-20 border border-bg-border rounded-md relative`}
    >
      <div className="overflow-auto">
        <Code showLines={false} code={code} language={language} canWrap />
      </div>
      <div
        className={`absolute ${
          code.split('\n').length > 1 ? 'top-4 right-4' : 'top-2.5 right-2.5'
        }`}
      >
        <Button
          variant="tertiary"
          size="small"
          onClick={() => {
            copyToClipboard(code);
            setCodeCopied(true);
            setTimeout(() => setCodeCopied(false), 2000);
          }}
        >
          {codeCopied ? <CheckIcon /> : <Clipboard />}
          {codeCopied ? 'Copied' : 'Copy'}
        </Button>
      </div>
    </div>
  );
};

export default NewCode;
